//! `AcpState`: the structured view session state, folded from `Event`s by a single writer.

use crate::daemon::PromptAttachmentRef;
use crate::session::Status;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

use super::approvals::{Approval, ApprovalDecision, Nonce};
use super::elicitations::{Elicitation, ElicitationAnswer, ElicitationOutcome};

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AcpSessionId(pub String);

/// Which backend agent is running this session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentName(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub title: String,
    pub detail: Option<String>,
    pub status: PlanStepStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub plan_id: String,
    pub version: u32,
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    pub id: String,
    pub text: String,
    pub completed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// ACP `ToolKind`, lowercased.
    #[serde(default)]
    pub kind: String,
    /// Capped at 16 KB at ingest, control chars stripped.
    pub args_preview: String,
    pub started_at: DateTime<Utc>,
    /// The sub-agent `Task` call this call runs under, from `_meta.claudeCode.parentToolUseId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_recall: Option<MemoryRecall>,
    /// File diffs attached via ACP `ToolCallContent::Diff`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diffs: Vec<DiffPreview>,
}

/// Structured payload for a `memory_recall` tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryRecall {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesized_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffPreview {
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// One renderable block of a tool call's completion, bridged from ACP `ToolCallContent`.
/// Binary `data` fields carry base64.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutputBlock {
    Text {
        text: String,
    },
    Image {
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    Audio {
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    Resource {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingSignal {
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RateLimitInfo {
    pub status: String,
    /// When the quota window clears, if the agent reported it.
    pub resets_at: Option<DateTime<Utc>>,
    pub kind: String,
}

impl RateLimitInfo {
    /// A park whose reporting `RateLimit` event was pruned: a limit with no reset time.
    pub fn undated() -> Self {
        Self {
            status: "limited".into(),
            resets_at: None,
            kind: "rate_limit".into(),
        }
    }
}

/// A live advisory the agent pushed outside the turn flow (approaching a rate
/// limit, a model fallback), per the ACP session-notices extension. Each
/// surface derives its own tone from the raw `severity` string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNotice {
    /// Minted from the event seq, so a client keys its local dismissal on it.
    pub id: String,
    pub severity: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A keyed notice keeps one id across updates; others are minted from the seq.
pub fn notice_id(key: Option<&str>, seq: u64) -> String {
    match key {
        Some(key) => format!("notice-{key}"),
        None => format!("notice-{seq}"),
    }
}

/// Snapshot of the most recent ACP agent handoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSwitchInfo {
    pub from: String,
    pub to: String,
    pub reason: String,
    pub switched_at: DateTime<Utc>,
}

/// The agent's last-reported context-window usage and cumulative cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUsage {
    pub used: u64,
    pub size: u64,
    #[serde(default)]
    pub cost: Option<UsageCost>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageCost {
    pub amount: f64,
    /// ISO 4217 code.
    pub currency: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionMode {
    #[default]
    Default,
    Plan,
    AcceptEdits,
    BypassPermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Title of the tool call the adapter opens for `/compact`; newer adapters emit
/// it instead of the `Compacting...` text chunk.
const COMPACT_TOOL_NAME: &str = "Compact conversation";

/// Agent-to-client notification carrying the agent's own auth identity.
pub const AUTH_STATUS_UPDATE_METHOD: &str = "_auth/status_update";

/// Which auth identity the agent process resolved for itself. An interim
/// `_meta` extension, so an unrecognised kind still renders from `label`
/// rather than dropping the whole report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStatusKind {
    Account,
    ApiKey,
    Gateway,
    External,
    /// The agent knows it is logged out. Distinct from never reporting.
    None,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStatusAccount {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Vendor plan string, not normalised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

/// The agent's own `_auth/status_update` payload. The upstream `vendor` bag is
/// deliberately not kept: nothing reads it, and it would persist unbounded
/// third-party data into the session log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStatus {
    pub kind: AuthStatusKind,
    /// Usable as a UI string on its own ("Claude Max", "Anthropic API key").
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AuthStatusAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub accepts_input: bool,
}

/// Semantic category of an ACP `SessionConfigOption`; unknown values round-trip as `Other`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigOptionCategory {
    Mode,
    Model,
    ThoughtLevel,
    #[serde(untagged)]
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigOptionChoice {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One ACP `SessionConfigOption` selector.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigOptionDescriptor {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub category: ConfigOptionCategory,
    pub current_value: String,
    pub options: Vec<ConfigOptionChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigOptionSwitchFailure {
    pub config_id: String,
    pub value: String,
    pub reason: String,
}

/// Why aoe refused a session whose adapter failed the compatibility check
/// after `initialize`. `auto_install` means "Update & restart" can fix it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StartupErrorDetail {
    IncompatibleAgentVersion {
        package_name: String,
        installed: String,
        required: String,
        install_command: String,
        #[serde(default)]
        auto_install: bool,
    },
    MissingAgentInfo {
        expected_package: String,
        install_command: String,
        #[serde(default)]
        auto_install: bool,
    },
    MismatchedAgentName {
        expected: String,
        received: String,
        install_command: String,
        #[serde(default)]
        auto_install: bool,
    },
    UnparseableAgentVersion {
        package_name: String,
        raw_version: String,
        required: String,
        install_command: String,
        #[serde(default)]
        auto_install: bool,
    },
    UnsupportedProtocolVersion {
        expected: String,
        received: String,
    },
}

/// This daemon's local setup/initialize, assignment, and established-command phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpConnectionPhase {
    Initializing,
    Establishing,
    Established,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpConnectionSource {
    Sdk,
    Control,
    Native,
    RunnerRecord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpConnectionCause {
    IncomingEnded,
    ReadFailed,
    WriteFailed,
    ProtocolEnded,
    DispatchFailed,
    ForwardPumpEnded,
    StdoutEof,
    ControlProtocolUnknown,
    ControlProtocolIncompatible,
    OwnershipUnconfirmed,
}

/// Why this daemon cannot use the connection; no process-death or recovery authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpConnectionUnavailability {
    pub source: AcpConnectionSource,
    pub cause: AcpConnectionCause,
    pub phase: AcpConnectionPhase,
}

impl AcpConnectionSource {
    /// Semantics of an already-qualified observation, not proof of process
    /// death: refused management attachment does not end retained native work.
    pub(crate) fn is_channel_loss(self) -> bool {
        matches!(self, Self::Sdk | Self::Control | Self::Native)
    }
}

/// Optional receipt metadata: missing and null both mean no affirmative receipt.
fn deserialize_optional_receipt<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(deserializer)?.unwrap_or(false))
}

/// Opaque incarnation equality, not permission to recover or create a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeEventContext([u8; 32]);

impl NativeEventContext {
    pub(crate) fn capture(
        row: &crate::session::Instance,
        storage: &crate::session::Storage,
    ) -> anyhow::Result<Self> {
        Self::capture_identity(&row.id, row.created_at, row.lifecycle_generation, storage)
    }

    pub(crate) fn capture_identity(
        id: &str,
        created_at: DateTime<Utc>,
        lifecycle_generation: u64,
        storage: &crate::session::Storage,
    ) -> anyhow::Result<Self> {
        use sha2::{Digest, Sha256};
        let (device, inode) = storage.bound_profile_identity()?;
        let mut digest = Sha256::new();
        digest.update(b"aoe-acp-native-context-v1\0");
        for text in [storage.profile(), id] {
            digest.update((text.len() as u64).to_le_bytes());
            digest.update(text.as_bytes());
        }
        digest.update(device.to_le_bytes());
        digest.update(inode.to_le_bytes());
        digest.update(created_at.timestamp().to_le_bytes());
        digest.update(created_at.timestamp_subsec_nanos().to_le_bytes());
        digest.update(lifecycle_generation.to_le_bytes());
        Ok(Self(digest.finalize().into()))
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) enum AcpSessionChange {
    Assigned(String),
}

impl AcpSessionChange {
    pub(crate) fn from_event(event: &Event) -> Option<Self> {
        match event {
            Event::AcpSessionAssigned { acp_session_id, .. } => {
                Some(Self::Assigned(acp_session_id.clone()))
            }
            _ => None,
        }
    }
}

/// Frozen host establishment identity, not permission or message authorship.
/// Actual RPC correlation remains separate from transport-row provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeEventOrigin {
    pub(crate) context: NativeEventContext,
    pub(crate) generation: u64,
    pub(crate) attempt_id: crate::acp::establishment::AttemptId,
    pub(crate) producer: uuid::Uuid,
    pub(crate) owner: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
}

impl NativeEventOrigin {
    pub(crate) fn owned_attempt(
        attempt: &crate::acp::establishment::EstablishmentAttempt,
        owner: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
    ) -> Option<Self> {
        Some(Self {
            context: attempt.context,
            generation: attempt.generation,
            attempt_id: attempt.id,
            producer: attempt.producer?,
            owner,
        })
    }

    pub(crate) fn same_physical_row(
        &self,
        row: &crate::session::Instance,
        storage: &crate::session::Storage,
    ) -> anyhow::Result<bool> {
        Ok(self.context
            == NativeEventContext::capture_identity(
                &row.id,
                row.created_at,
                self.generation,
                storage,
            )?)
    }
}

/// Authenticated Native transport and physical row witness only.
/// The witnessed attempt does not identify who authored a content message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativePhysicalSource {
    pub(crate) witness: NativeEventOrigin,
}

impl NativePhysicalSource {
    pub(crate) fn from_receipt(
        receipt: &crate::acp::establishment::NativeEstablishmentReceipt,
    ) -> Option<Self> {
        receipt.owner.birth_fingerprint?;
        Some(Self {
            witness: NativeEventOrigin::owned_attempt(&receipt.attempt, Some(receipt.owner))?,
        })
    }
}

/// Exact metadata selection at a controller checkpoint. The general
/// conversation equality intentionally omits ACP identity and route provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBindingState {
    pub(crate) choice: crate::session::ConversationState,
    pub(crate) acp_session_id: Option<String>,
    pub(crate) fork_pending: Option<String>,
    pub(crate) import_pending: Option<bool>,
    pub(crate) project_path: String,
    pub(crate) agent_name: Option<String>,
    pub(crate) agent_provider: Option<String>,
    pub(crate) authority: Option<crate::acp::establishment::AuthorityKey>,
    pub(crate) sandboxed: bool,
}

impl NativeBindingState {
    pub(crate) fn capture(row: &crate::session::Instance) -> Self {
        Self {
            choice: row.conversation_state(),
            acp_session_id: row.acp_session_id.clone(),
            fork_pending: row.fork_pending.clone(),
            import_pending: row.import_pending,
            project_path: row.project_path.clone(),
            agent_name: row.agent_name.clone(),
            agent_provider: row.agent_provider.clone(),
            authority: row.acp_establishment.as_ref().map(|state| state.key()),
            sandboxed: row.is_sandboxed(),
        }
    }

    pub(crate) fn matches(&self, row: &crate::session::Instance) -> bool {
        self.selection_matches(row)
            && self.authority == row.acp_establishment.as_ref().map(|state| state.key())
    }

    pub(crate) fn selection_matches(&self, row: &crate::session::Instance) -> bool {
        self.choice.matches_checkpoint(row)
            && self.acp_session_id == row.acp_session_id
            && self.fork_pending == row.fork_pending
            && self.import_pending == row.import_pending
            && self.project_path == row.project_path
            && self.agent_name == row.agent_name
            && self.agent_provider == row.agent_provider
            && self.sandboxed == row.is_sandboxed()
    }

    pub(crate) fn same_selection(&self, other: &Self) -> bool {
        self.choice.same_checkpoint(&other.choice)
            && self.acp_session_id == other.acp_session_id
            && self.fork_pending == other.fork_pending
            && self.import_pending == other.import_pending
            && self.project_path == other.project_path
            && self.agent_name == other.agent_name
            && self.agent_provider == other.agent_provider
            && self.sandboxed == other.sandboxed
    }
}

impl PartialEq for NativeBindingState {
    fn eq(&self, other: &Self) -> bool {
        self.same_selection(other) && self.authority == other.authority
    }
}

impl Eq for NativeBindingState {}

#[derive(Debug, Clone)]
pub(crate) struct NativeObservationScope {
    pub context: NativeEventContext,
    pub choice: Option<std::sync::Arc<NativeBindingState>>,
    pub fence: std::sync::Arc<crate::session::NativeInputFence>,
}

impl NativeObservationScope {
    pub fn capture(
        row: &crate::session::Instance,
        storage: &crate::session::Storage,
    ) -> anyhow::Result<Self> {
        Self::capture_choice(
            row,
            storage,
            std::sync::Arc::new(NativeBindingState::capture(row)),
        )
    }

    pub(crate) fn retarget(
        &self,
        row: &crate::session::Instance,
        storage: &crate::session::Storage,
    ) -> anyhow::Result<Self> {
        let choice = self
            .choice
            .as_ref()
            .filter(|choice| choice.matches(row))
            .cloned()
            .unwrap_or_else(|| std::sync::Arc::new(NativeBindingState::capture(row)));
        Self::capture_choice(row, storage, choice)
    }

    fn capture_choice(
        row: &crate::session::Instance,
        storage: &crate::session::Storage,
        choice: std::sync::Arc<NativeBindingState>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            context: NativeEventContext::capture(row, storage)?,
            choice: Some(choice),
            fence: crate::session::NativeInputFence::capture_route(
                storage.profile(),
                &row.id,
                row.created_at,
                row.lifecycle_generation,
                false,
                Some(storage.bound_profile_identity()?),
            )?,
        })
    }
}

/// Kept only in the private journal and runtime fold, never public JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeEventSubject {
    pub(crate) context: NativeEventContext,
    pub(crate) owner: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
    pub(crate) kind: NativeSubjectEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum NativeSubjectEvent {
    ChannelReady,
    BindingObserved,
    RuntimeObservation,
    StartupDiagnostic,
    ChannelUnavailable(AcpConnectionUnavailability),
    PromptDiagnostic,
    TurnEnded,
    AdmissionRefusal(crate::process::worker_registry::ControlAdmissionRefusal),
}

impl NativeEventSubject {
    pub(crate) fn channel(
        context: NativeEventContext,
        owner: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
        event: &Event,
    ) -> Self {
        let kind = match event {
            Event::AcpSessionAssigned {
                connection_ready, ..
            } => {
                if *connection_ready {
                    NativeSubjectEvent::ChannelReady
                } else {
                    NativeSubjectEvent::BindingObserved
                }
            }
            Event::AcpConnectionUnavailable {
                source,
                cause,
                phase,
                ..
            } => NativeSubjectEvent::ChannelUnavailable(AcpConnectionUnavailability {
                source: *source,
                cause: *cause,
                phase: *phase,
            }),
            Event::PromptRuntimeError { .. } => NativeSubjectEvent::PromptDiagnostic,
            Event::Stopped { .. } => NativeSubjectEvent::TurnEnded,
            Event::AgentStartupError { .. } => NativeSubjectEvent::StartupDiagnostic,
            _ => NativeSubjectEvent::RuntimeObservation,
        };
        Self {
            context,
            owner,
            kind,
        }
    }

    pub(crate) fn owner_is_current(&self, session_id: &str) -> bool {
        if let NativeSubjectEvent::AdmissionRefusal(refusal) = self.kind {
            return crate::process::worker_registry::refusal_still_applies(session_id, &refusal);
        }
        self.owner.is_some_and(|owner| {
            crate::process::worker_registry::load_strict(session_id)
                .ok()
                .flatten()
                .is_some_and(|record| owner.owns_record(&record))
                && crate::process::process_birth_identity(owner.pid)
                    .ok()
                    .flatten()
                    .is_some_and(|birth| Some(birth.fingerprint()) == owner.birth_fingerprint)
        })
    }

    pub(crate) fn owner_not_replaced(&self, session_id: &str) -> bool {
        let Some(owner) = self.owner.filter(|owner| owner.birth_fingerprint.is_some()) else {
            return false;
        };
        match crate::process::worker_registry::load_strict(session_id) {
            Ok(Some(record)) => owner.owns_record(&record),
            Ok(None) => true,
            Err(_) => false,
        }
    }

    pub(crate) fn unavailability_still_applies(&self, session_id: &str) -> bool {
        match self.kind {
            NativeSubjectEvent::AdmissionRefusal(refusal) => {
                crate::process::worker_registry::refusal_still_applies(session_id, &refusal)
            }
            NativeSubjectEvent::ChannelUnavailable(_) => self.owner_not_replaced(session_id),
            _ => false,
        }
    }
}

#[derive(Clone)]
pub(crate) struct CurrentNativeAvailability {
    pub scope: NativeObservationScope,
    pub owner: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
    pub unavailable: Option<AcpConnectionUnavailability>,
    pub ready: bool,
    pub client: std::sync::Arc<crate::acp::acp_client::AcpClient>,
    pub lifecycle: std::sync::Arc<std::sync::Mutex<crate::acp::runner_lifecycle::LifecycleTable>>,
    pub lease: crate::acp::runner_lifecycle::Lease,
}

impl CurrentNativeAvailability {
    pub(crate) fn same_actor(&self, other: &Self) -> bool {
        self.scope.context == other.scope.context
            && self.scope.choice == other.scope.choice
            && self.owner == other.owner
            && self.lease == other.lease
            && std::sync::Arc::ptr_eq(&self.client, &other.client)
    }

    /// Refresh lifecycle/client facts under the Native fence; owner proof is separate.
    /// `selected` compares this frozen choice with the fenced row.
    pub(crate) fn refresh(
        &mut self,
        session_id: &str,
        selected: bool,
    ) -> Result<bool, NativeObservationWait> {
        let lifecycle = match self.lifecycle.try_lock() {
            Ok(lifecycle) => lifecycle,
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(NativeObservationWait {
                    context: self.scope.context,
                    lock: NativeObservationLock::Lifecycle(self.lifecycle.clone()),
                });
            }
            Err(std::sync::TryLockError::Poisoned(_)) => return Ok(false),
        };
        if lifecycle.phase(session_id) != crate::acp::runner_lifecycle::WorkerPhase::Running
            || lifecycle.current_lease(session_id).as_ref() != Some(&self.lease)
            || self.owner != self.client.connected_runner()
        {
            return Ok(false);
        }
        self.ready = selected && self.client.connection_ready();
        self.unavailable = self.client.connection_unavailable();
        self.ready &= self.unavailable.is_none();
        Ok(self.ready || self.unavailable.is_some())
    }

    /// After refresh, qualify ownership under the Native fence without a registry guard.
    pub(crate) fn qualify_owner(&self, session_id: &str) -> Result<bool, NativeObservationWait> {
        let Some(owner) = self.owner.filter(|owner| owner.birth_fingerprint.is_some()) else {
            return Ok(false);
        };
        let record = match crate::process::worker_registry::load_strict(session_id) {
            Ok(record) => record,
            Err(error) if error.is::<crate::process::worker_registry::RegistryBusy>() => {
                return Err(NativeObservationWait {
                    context: self.scope.context,
                    lock: NativeObservationLock::Registry(session_id.to_owned()),
                });
            }
            Err(_) => return Ok(false),
        };
        if self.unavailable.is_some() {
            return Ok(record.is_none_or(|record| owner.owns_record(&record)));
        }
        Ok(record.is_some_and(|record| owner.owns_record(&record))
            && crate::process::process_birth_identity(owner.pid)
                .ok()
                .flatten()
                .is_some_and(|birth| Some(birth.fingerprint()) == owner.birth_fingerprint))
    }
}

pub(crate) struct NativeObservationWait {
    pub context: NativeEventContext,
    lock: NativeObservationLock,
}

impl std::fmt::Debug for NativeObservationWait {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeObservationWait")
            .finish_non_exhaustive()
    }
}

enum NativeObservationLock {
    Native(std::sync::Arc<crate::session::NativeInputFence>),
    Lifecycle(std::sync::Arc<std::sync::Mutex<crate::acp::runner_lifecycle::LifecycleTable>>),
    Registry(String),
}

impl NativeObservationWait {
    pub(crate) fn native(scope: &NativeObservationScope) -> Self {
        Self {
            context: scope.context,
            lock: NativeObservationLock::Native(scope.fence.clone()),
        }
    }

    /// Only the contended lock survives into this wait, never a publication guard.
    pub(crate) async fn settle(self) -> bool {
        tokio::task::spawn_blocking(move || match self.lock {
            NativeObservationLock::Native(fence) => fence.lock_observation().is_ok(),
            NativeObservationLock::Lifecycle(lifecycle) => lifecycle.lock().is_ok(),
            NativeObservationLock::Registry(session_id) => {
                crate::process::worker_registry::acquire_registry_lock(&session_id).is_ok()
            }
        })
        .await
        .unwrap_or(false)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NativeAvailabilityProjection {
    pub ready: bool,
    pub unavailable: Option<AcpConnectionUnavailability>,
    pub control_failure: Option<(u64, NativeControlFailure)>,
    pub prompt_wake_eligible: bool,
    context: Option<NativeEventContext>,
    owner: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeControlFailure {
    Prompt,
    Startup,
}

impl NativeAvailabilityProjection {
    pub(crate) fn resolve(
        historical: Option<(u64, NativeEventSubject)>,
        context: Option<NativeEventContext>,
        current: Option<&CurrentNativeAvailability>,
        historical_owner_current: bool,
        current_refusal: Option<crate::process::worker_registry::ControlAdmissionRefusal>,
    ) -> Self {
        let Some(context) = context else {
            return Self::default();
        };
        if let Some(current) = current.filter(|current| current.scope.context == context) {
            return Self {
                ready: current.ready && current.unavailable.is_none(),
                unavailable: current.unavailable,
                control_failure: None,
                prompt_wake_eligible: false,
                context: Some(context),
                owner: current.owner,
            };
        }
        let unavailable = if let Some(refusal) = current_refusal {
            Some(refusal.observation())
        } else {
            historical
                .filter(|(_, subject)| subject.context == context && historical_owner_current)
                .and_then(|(_, subject)| match subject.kind {
                    NativeSubjectEvent::ChannelUnavailable(unavailable)
                        if unavailable.source == AcpConnectionSource::Native =>
                    {
                        Some(unavailable)
                    }
                    NativeSubjectEvent::AdmissionRefusal(refusal) => Some(refusal.observation()),
                    _ => None,
                })
        };
        Self {
            ready: false,
            unavailable,
            control_failure: None,
            context: Some(context),
            prompt_wake_eligible: false,
            owner: current_refusal
                .and_then(|refusal| refusal.owner)
                .or_else(|| historical.and_then(|(_, subject)| subject.owner)),
        }
    }

    pub(crate) fn with_prompt_wake_eligibility(mut self, eligible: bool) -> Self {
        self.prompt_wake_eligible =
            eligible && self.context.is_some() && !self.ready && self.unavailable.is_none();
        self
    }

    pub(crate) fn context(&self) -> Option<NativeEventContext> {
        self.context
    }
}

impl Event {
    /// Admission refusal describes management, not terminal native work.
    pub(crate) fn is_channel_loss(&self) -> bool {
        matches!(self, Self::AcpConnectionUnavailable { source, native_subject, .. }
            if source.is_channel_loss()
                && !native_subject.is_some_and(|subject| matches!(subject.kind, NativeSubjectEvent::AdmissionRefusal(_))))
    }

    pub(crate) fn native_subject(&self) -> Option<NativeEventSubject> {
        match self {
            Self::PromptRuntimeError { native_subject, .. }
            | Self::AcpConnectionUnavailable { native_subject, .. }
            | Self::AgentStartupError { native_subject, .. }
            | Self::AcpSessionAssigned { native_subject, .. } => *native_subject,
            _ => None,
        }
    }

    pub(crate) fn set_native_subject(&mut self, subject: NativeEventSubject) {
        match self {
            Self::PromptRuntimeError { native_subject, .. }
            | Self::AcpConnectionUnavailable { native_subject, .. }
            | Self::AgentStartupError { native_subject, .. }
            | Self::AcpSessionAssigned { native_subject, .. } => *native_subject = Some(subject),
            _ => {}
        }
    }
}

/// Lifecycle status of an async background sub-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundAgentStatus {
    Running,
    /// No transcript growth for the idle window.
    Stalled,
    Completed,
    /// The parent session ended before the agent finished.
    Detached,
    /// The transcript could not be read or parsed.
    Error,
}

impl BackgroundAgentStatus {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Detached | Self::Error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTaskLaunch {
    pub task_id: String,
    pub tool_call_id: Option<String>,
    pub workflow: Option<WorkflowLaunch>,
    /// What the log already recorded for the workflow's agents, so a resumed
    /// journal read neither repeats nor misses an event.
    pub workflow_agents: WorkflowAgents,
    pub(crate) witness: Option<Arc<crate::acp::establishment::NativeResourceWitness>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowAgents {
    pub launched: std::collections::HashSet<String>,
    pub completed: std::collections::HashSet<String>,
    pub(crate) resources: HashMap<String, Arc<crate::acp::establishment::NativeResourceWitness>>,
}

/// A Claude `Workflow` run, from its launch result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowLaunch {
    pub run_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Host directory holding the run's journal and agent transcripts.
    /// Stripped before any client-facing frame, like
    /// `BackgroundAgentLaunched::output_file`.
    #[serde(default)]
    pub transcript_dir: String,
}

/// A workflow run of the session, kept like `background_agents` so a client
/// whose replay window misses its launch or finish still ties the task to its
/// tool call and end time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunRecord {
    pub task_id: String,
    pub tool_call_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Closed by its task's finish, or by a later start reusing the task id.
    #[serde(default)]
    pub finished: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
}

/// The workflow run and phase a background sub-agent belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRef {
    pub run_id: String,
    pub name: String,
    pub phase: String,
}

/// One tool call a background sub-agent made, parsed from its transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundAgentTool {
    pub name: String,
    /// Short label from the tool input (command, file path, pattern).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `None` while running, then whether it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
}

/// One async background sub-agent, built up from `BackgroundAgent*` events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundAgentRecord {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_ref: Option<uuid::Uuid>,
    /// The parent `Task` tool call that launched this agent.
    pub tool_call_id: String,
    pub description: String,
    pub prompt: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowRef>,
    pub status: BackgroundAgentStatus,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub tool_count: u32,
    #[serde(default)]
    pub tools: Vec<BackgroundAgentTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<String>,
    /// Preview of the most recent assistant text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_text: Option<String>,
    /// The terminal assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Non-fatal note, e.g. an unrecognized transcript format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// What the main turn is doing for the sidebar. Kept apart from `turn_active`,
/// which dispatch reads and which must not follow display-only signals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SidebarPhase {
    #[default]
    Idle,
    Active,
    Waiting,
    Error,
    PromptFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTaskRecord {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_ref: Option<uuid::Uuid>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AcpState {
    pub session_id: AcpSessionId,
    pub agent: AgentName,
    pub model: Option<String>,
    pub mode: SessionMode,

    pub current_plan: Option<Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_plan_seq: Option<u64>,
    pub todos: Vec<Todo>,
    pub in_flight_tool: Option<ToolCall>,
    pub pending_approvals: Vec<Approval>,
    #[serde(default)]
    pub pending_elicitations: Vec<Elicitation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub original_decision_nonces: Vec<Nonce>,
    #[serde(default)]
    pub recent_diffs: Vec<DiffPreview>,
    pub thinking: Option<ThinkingSignal>,
    pub rate_limit: Option<RateLimitInfo>,
    #[serde(default)]
    pub usage: Option<SessionUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_usage: Option<SessionUsage>,
    #[serde(skip)]
    operation_effects: crate::acp::operation_effect::OperationEffects,
    #[serde(default)]
    pub available_commands: Vec<AvailableCommand>,
    /// Modes the adapter advertised; adapter-scoped, so they survive `/clear`.
    #[serde(default)]
    pub available_modes: Vec<ModeInfo>,
    #[serde(default)]
    pub current_mode_id: Option<String>,
    /// Identity the adapter reported for itself. `None` means it never
    /// reported, which the UI shows as nothing rather than as logged out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_status: Option<AuthStatus>,
    #[serde(default)]
    pub last_agent_switch: Option<AgentSwitchInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_error: Option<StartupErrorDetail>,
    /// The diagnostic message stays in the event/transcript, independently of liveness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_error: Option<crate::session::OperationErrorEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_diagnostic: Option<String>,
    #[serde(skip)]
    operation_diagnostic_origin: Option<NativeEventOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_diagnostic: Option<String>,
    #[serde(default)]
    pub operation_output_uncertain: bool,
    /// Live server overlay only; cached data never supplies action authority.
    #[serde(default, skip_serializing)]
    pub recovery: Option<crate::session::SessionRecovery>,
    /// Missing means no recorded unavailability, not affirmative native-channel health.
    #[serde(default)]
    pub connection_unavailable: Option<AcpConnectionUnavailability>,
    /// Only a current, source-qualified channel observation makes this true.
    #[serde(default, deserialize_with = "deserialize_optional_receipt")]
    pub connection_ready: bool,
    /// Live reduced-state envelope metadata for clients only. The server derives
    /// its governor from NativeAvailabilityProjection, never this deserialized
    /// field. Missing/null is unknown; history/cache serialization omits it.
    #[serde(default, skip_serializing)]
    pub(crate) prompt_wake_eligible: Option<bool>,
    #[serde(skip)]
    pub(crate) native_availability: Option<(u64, NativeEventSubject)>,
    #[serde(skip)]
    pub(crate) native_control_seq: u64,
    /// Full snapshot of the adapter's per-session selectors.
    #[serde(default)]
    pub config_options: Vec<ConfigOptionDescriptor>,
    /// Notice for the most recent rejected `session/set_config_option`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_option_switch_failed: Option<ConfigOptionSwitchFailure>,
    /// Undismissed advisories for the current turn; the next prompt clears them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_notices: Vec<SessionNotice>,
    #[serde(default)]
    pub background_agents: Vec<BackgroundAgentRecord>,
    /// Ids of running background tasks that are not sub-agents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_tasks: Vec<BackgroundTaskRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_runs: Vec<WorkflowRunRecord>,

    /// Whether the main turn is in flight. Dispatch (`acp::dispatch::decide`)
    /// and the queue drain gate on this directly, so it tracks only the main
    /// turn, never a background sub-agent; display signals combine it with
    /// `has_active_background_agent()`.
    #[serde(default)]
    pub turn_active: bool,
    /// Sidebar projection input; rebuilt from the log, so never serialized.
    #[serde(skip)]
    pub sidebar_phase: SidebarPhase,
    /// Derived lifecycle/resource observation; zero means the default phase is not evidence.
    #[serde(skip)]
    pub(crate) sidebar_observation_seq: u64,
    #[serde(skip)]
    pub(crate) sidebar_observation_origin: Option<NativeEventOrigin>,
    /// A mid-turn prompt is injected into the running turn rather than queued.
    #[serde(default)]
    pub steering: bool,
    /// A cancel was requested and its terminal `Stopped` has not arrived.
    #[serde(default)]
    pub cancelling: bool,
    /// A `/compact` is running; the adapter goes silent for minutes.
    #[serde(default)]
    pub compacting: bool,
    /// The adapter's `/compact` tool call, whose completion ends `compacting`.
    /// Held by id because a later update can retitle the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_tool_id: Option<String>,

    pub last_seq: u64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("approval nonce {0:?} did not match any pending approval")]
    UnknownApprovalNonce(Nonce),
    #[error("approval nonce {0:?} already resolved")]
    ApprovalAlreadyResolved(Nonce),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffComment {
    pub id: String,
    /// Workspace member name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    pub file_path: String,
    /// `"old"` or `"new"`; a string so an unknown side cannot fail replay.
    pub side: String,
    pub start_line: u32,
    pub end_line: u32,
    pub body: String,
    pub captured_snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// Terminal park reason once rate-limit auto-resume exhausts its redelivery budget.
pub(crate) const RATE_LIMIT_EXHAUSTED_RETRIES_REASON: &str = "rate_limit_exhausted_retries";

/// A state mutation, persisted verbatim in the event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    PlanUpdated {
        plan: Plan,
    },
    TodoListUpdated {
        todos: Vec<Todo>,
    },
    /// Legacy: agent-pushed `session_info_update` titles.
    SessionTitleSuggested {
        title: String,
    },
    ToolCallStarted {
        tool_call: ToolCall,
    },
    ToolCallCompleted {
        tool_call_id: String,
        is_error: bool,
        /// Concatenated text blocks of the final `ToolCallUpdate` content.
        #[serde(default)]
        content: String,
        /// Structured completion blocks (images, audio, resources, text).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        output: Vec<ToolOutputBlock>,
        #[serde(default = "chrono::Utc::now")]
        completed_at: DateTime<Utc>,
        #[serde(default)]
        async_subagent: bool,
    },
    ToolCallContent {
        tool_call_id: String,
        content: String,
    },
    /// Late-arriving fields for an in-flight tool call.
    ToolCallUpdated {
        tool_call_id: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        args_preview: Option<String>,
        #[serde(default)]
        started_at: Option<DateTime<Utc>>,
        /// Replaces the call's diffs when present (Codex sends them on updates).
        #[serde(default)]
        diffs: Option<Vec<DiffPreview>>,
    },
    ApprovalRequested {
        approval: Approval,
    },
    ApprovalResolved {
        nonce: Nonce,
        decision: ApprovalDecision,
    },
    /// The agent asked a structured question (`AskUserQuestion` via `elicitation/create`).
    ElicitationRequested {
        elicitation: Elicitation,
    },
    ElicitationResolved {
        nonce: Nonce,
        outcome: ElicitationOutcome,
        #[serde(default)]
        answers: Vec<ElicitationAnswer>,
    },
    DiffEmitted {
        diff: DiffPreview,
    },
    ThinkingStarted,
    ThinkingEnded,
    RateLimit {
        info: RateLimitInfo,
    },
    /// An ACP session notice: a live advisory, not conversation history.
    SessionNotice {
        /// Raw wire severity, so a future ACP level survives the log unchanged.
        severity: String,
        title: String,
        #[serde(default)]
        description: Option<String>,
        /// A later notice with the same key replaces this one in place.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// Auto-resume breadcrumb; `manual` when the user pressed RESUME NOW.
    RateLimitAutoResumed {
        resets_at: DateTime<Utc>,
        #[serde(default)]
        manual: bool,
    },
    UsageUpdated {
        usage: SessionUsage,
    },
    ModeChanged {
        mode: SessionMode,
    },
    ModesAvailable {
        current_mode_id: String,
        modes: Vec<ModeInfo>,
    },
    CurrentModeChanged {
        current_mode_id: String,
    },
    ModeSwitchFailed {
        mode_id: String,
        reason: String,
    },
    AvailableCommandsUpdated {
        commands: Vec<AvailableCommand>,
    },
    ConfigOptionsUpdated {
        options: Vec<ConfigOptionDescriptor>,
    },
    ConfigOptionSwitchFailed {
        config_id: String,
        value: String,
        reason: String,
    },
    /// The agent reported which auth identity it runs under. `None` clears a
    /// report inherited from an earlier adapter process that this one cannot
    /// refresh; see `AcpState::auth_status`.
    AuthStatusUpdated {
        status: Option<AuthStatus>,
    },
    /// An ACP `session/update` payload with no typed variant yet.
    RawAgentUpdate {
        payload: serde_json::Value,
    },
    /// An async sub-agent (Claude `Task` with isAsync) was launched.
    BackgroundAgentLaunched {
        agent_id: String,
        tool_call_id: String,
        description: String,
        prompt: String,
        model: String,
        /// Local transcript path the daemon tails. Persisted so a daemon that
        /// restarts mid-run can re-tail a sub-agent that survived it; an event
        /// from before this field existed defaults to empty, which the resume
        /// sweep treats as untrackable and detaches. A host fs path, stripped
        /// before any client-facing frame (`protocol::strip_transcript_path`).
        #[serde(default)]
        output_file: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<WorkflowRef>,
        started_at: DateTime<Utc>,
    },
    /// Throttled snapshot of a running background sub-agent.
    BackgroundAgentProgress {
        agent_id: String,
        status: BackgroundAgentStatus,
        tool_count: u32,
        #[serde(default)]
        tools: Vec<BackgroundAgentTool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_tool: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_text: Option<String>,
        at: DateTime<Utc>,
    },
    /// Terminal state for a background sub-agent.
    BackgroundAgentCompleted {
        agent_id: String,
        status: BackgroundAgentStatus,
        #[serde(default)]
        tools: Vec<BackgroundAgentTool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
        ended_at: DateTime<Utc>,
    },
    /// A background task that is not a sub-agent (Bash `run_in_background`, or
    /// a `Workflow` run) started, from the tool result's `toolResponse`. Its
    /// end is read from the session transcript (`acp::task_transcript`).
    BackgroundTaskStarted {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        description: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<WorkflowLaunch>,
    },
    /// A background task reached a terminal state.
    BackgroundTaskFinished {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ended_at: Option<DateTime<Utc>>,
    },
    /// A prompt operation failed, possibly after transcript or tool output.
    /// This does not establish process death or invalidate conversation history.
    PromptRuntimeError {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<crate::session::OperationErrorEvidence>,
        #[serde(default, skip_serializing)]
        native_subject: Option<NativeEventSubject>,
    },
    /// The local owner can no longer use this ACP connection.
    AcpConnectionUnavailable {
        source: AcpConnectionSource,
        cause: AcpConnectionCause,
        phase: AcpConnectionPhase,
        #[serde(default, skip_serializing)]
        native_subject: Option<NativeEventSubject>,
    },
    AgentMessageChunk {
        text: String,
        /// The ACP message this chunk belongs to; a change starts a new message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    /// aoe sent `session/cancel` and armed the escalation watchdog.
    CancelRequested {
        escalates_at: DateTime<Utc>,
    },
    Stopped {
        reason: String,
    },
    /// The agent failed to spawn or never completed `initialize`.
    AgentStartupError {
        message: String,
        #[serde(default, skip_serializing)]
        native_subject: Option<NativeEventSubject>,
    },
    /// `initialize` completed but the adapter failed the compatibility policy.
    IncompatibleAgent {
        detail: StartupErrorDetail,
    },
    UserPromptSent {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<PromptAttachmentRef>,
        /// Client-minted id so a web client reconciles its optimistic row.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_id: Option<String>,
        /// True when the daemon queued this turn itself (a rate-limit resume
        /// continuation) rather than the user typing it just now. The
        /// transcript model skips rendering a row for it: the user already
        /// saw this text once, before the park. `#[serde(default)]` keeps
        /// pre-existing persisted events deserialising as non-synthesized.
        #[serde(default)]
        synthesized: bool,
        /// Id of the plugin that sent this through `sessions.message.send`; the
        /// text was not typed by the user.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin_plugin: Option<String>,
    },
    PromptCapabilities {
        image: bool,
        audio: bool,
        embedded_context: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        load_session: Option<bool>,
        /// Whether the agent accepts `_session/steering` mid-turn prompts.
        #[serde(default)]
        steering: bool,
    },
    /// A "Send diff comments" submission; `assembled_markdown` is what the agent receives.
    #[serde(rename_all = "camelCase")]
    UserDiffCommentsPrompt {
        intro: String,
        outro: String,
        is_multi_repo: bool,
        comments: Vec<DiffComment>,
        assembled_markdown: String,
    },
    /// A prompt arrived while another `session/prompt` was in flight.
    PromptRejected {
        reason: String,
        text: String,
    },
    /// Native ACP session id admitted by a successful new, load, fork, or
    /// resume. The server listener persists it on `Instance.acp_session_id` so
    /// the next spawn can `session/load`.
    AcpSessionAssigned {
        acp_session_id: String,
        /// Qualified local admission, not registry identity or historical backfill.
        #[serde(
            default,
            deserialize_with = "deserialize_optional_receipt",
            skip_serializing_if = "std::ops::Not::not"
        )]
        connection_ready: bool,
        #[serde(default, skip_serializing)]
        native_subject: Option<NativeEventSubject>,
    },
    /// A new native context boundary, without erasing transcript history.
    SessionContextReset {
        reason: String,
    },
    /// The agent called the Claude SDK's `ScheduleWakeup` tool.
    WakeupScheduled {
        at: DateTime<Utc>,
        reason: Option<String>,
    },
    /// The agent armed the Claude SDK's `Monitor` tool.
    MonitorArmed {
        description: Option<String>,
    },
    /// The conversation was cleared (`/clear` or a driven reset).
    SessionCleared,
    /// `/compact` started; the adapter goes silent while it summarizes.
    ConversationCompactionStarted,
    /// `/compact` replaced the model's context with a summary.
    ConversationCompacted,
    /// A compaction ended without replacing the context.
    ConversationCompactionFailed,
    /// The summary the agent retained when compacting, after its
    /// `ConversationCompacted`. A later one for the same compaction replaces
    /// it; empty `text` clears it.
    ConversationCompactionSummary {
        compaction_id: String,
        text: String,
    },
    AgentSwitched {
        from: String,
        to: String,
        reason: String,
    },
    /// An aoe-generated recap of the conversation so far.
    ConversationSummary {
        text: String,
        summarized_until_seq: u64,
    },
}

impl AcpState {
    const MAX_RECENT_DIFFS: usize = 16;
    /// A chatty turn must not push the banner past the strip's useful height.
    const MAX_SESSION_NOTICES: usize = 3;

    pub fn new(session_id: AcpSessionId, agent: AgentName, model: Option<String>) -> Self {
        Self {
            session_id,
            agent,
            model,
            updated_at: Utc::now(),
            ..Self::default()
        }
    }

    /// Whether any background sub-agent or task is still in flight. Agents
    /// are keyed on `ended_at`, not `status`: a terminal
    /// `BackgroundAgentCompleted` can carry `status: Stalled` (the tailer's
    /// abort timeout gives up without a clean `end_turn`), and that record is
    /// done like any other (#4001).
    pub fn has_active_background_agent(&self) -> bool {
        !self.background_tasks.is_empty()
            || self.background_agents.iter().any(|a| a.ended_at.is_none())
    }

    /// Task ids can be reused, so only the run still open on `task_id` closes.
    fn close_workflow_run(&mut self, task_id: &str, ended_at: Option<DateTime<Utc>>) {
        if let Some(run) = self
            .workflow_runs
            .iter_mut()
            .find(|run| run.task_id == task_id && !run.finished)
        {
            run.finished = true;
            run.ended_at = ended_at;
        }
    }

    /// Historical notices never grant current channel health or recovery authority.
    pub(crate) fn apply_native_availability(&mut self, projection: NativeAvailabilityProjection) {
        if projection.ready
            && projection.unavailable.is_none()
            && projection.owner.is_some()
            && self.sidebar_phase == SidebarPhase::Error
            && self.operation_error.is_none()
            && self.operation_diagnostic.is_none()
            && self.native_availability.is_some_and(|(_, subject)| {
                Some(subject.context) == projection.context
                    && subject.owner == projection.owner
                    && matches!(
                        subject.kind,
                        NativeSubjectEvent::ChannelUnavailable(unavailable)
                            if unavailable.source.is_channel_loss()
                    )
            })
        {
            // Only qualified current readiness can heal this same owner's loss.
            self.heal_sidebar_phase();
        }
        self.connection_ready = projection.ready;
        self.connection_unavailable = projection.unavailable;
        if let Some((seq, failure)) = projection
            .control_failure
            .filter(|(seq, _)| *seq > self.native_control_seq)
        {
            self.native_control_seq = seq;
            self.sidebar_observation_seq = seq;
            self.sidebar_observation_origin = None;
            self.end_turn("aborted");
            if failure == NativeControlFailure::Startup {
                self.sidebar_phase = SidebarPhase::Error;
            }
        }
        if self
            .connection_unavailable
            .is_some_and(|unavailable| unavailable.source.is_channel_loss())
        {
            self.retire_unavailable_controls();
        }
    }

    /// Qualified main-turn status plus genuinely allocated background work, never raw resource history.
    pub fn sidebar_status(&self) -> Status {
        if self.connection_unavailable.is_some() {
            return Status::Error;
        }
        match self.sidebar_phase {
            SidebarPhase::Error | SidebarPhase::PromptFailed => Status::Error,
            SidebarPhase::Waiting => Status::Waiting,
            SidebarPhase::Active => Status::Running,
            SidebarPhase::Idle if self.has_qualified_background_work() => Status::Running,
            SidebarPhase::Idle => Status::Idle,
        }
    }

    pub(crate) fn has_qualified_background_work(&self) -> bool {
        self.background_tasks
            .iter()
            .any(|task| task.resource_ref.is_some())
            || self
                .background_agents
                .iter()
                .any(|agent| agent.resource_ref.is_some() && agent.ended_at.is_none())
    }

    pub(crate) fn apply_scoped_event(
        &mut self,
        seq: u64,
        event: &Event,
        metadata: &crate::acp::event_sender::EventMetadata,
    ) -> Result<u64, StateError> {
        use crate::acp::operation_effect::ControlKind;
        let previous = self.operation_effects.active();
        let kind = ControlKind::from_event(event);
        let effect = self.operation_effects.apply(kind, metadata);
        if effect.applies_state() {
            if matches!(kind, ControlKind::Admission)
                && self.operation_effects.active().is_some()
                && self.operation_effects.active() != previous
            {
                self.open_turn();
                self.native_control_seq = self.native_control_seq.max(seq);
            }
            if metadata.output_uncertainty.is_some()
                && matches!(
                    event,
                    Event::PromptRuntimeError { .. } | Event::Stopped { .. }
                )
            {
                self.operation_output_uncertain = true;
            }
            return self.apply_current_event(
                seq,
                event,
                kind,
                metadata.native_origin,
                metadata.clear_after_seq,
            );
        } else {
            match event {
                Event::PlanUpdated { .. }
                | Event::TodoListUpdated { .. }
                | Event::DiffEmitted { .. }
                | Event::SessionNotice { .. } => {
                    return self.apply_current_event(
                        seq,
                        event,
                        kind,
                        metadata.native_origin,
                        metadata.clear_after_seq,
                    );
                }
                Event::UsageUpdated { usage } => self.historical_usage = Some(usage.clone()),
                Event::ApprovalResolved { nonce, .. } => self
                    .pending_approvals
                    .retain(|approval| approval.nonce != *nonce),
                Event::ElicitationResolved { nonce, .. } => self
                    .pending_elicitations
                    .retain(|elicitation| elicitation.nonce != *nonce),
                Event::BackgroundAgentLaunched { agent_id, .. } => {
                    let existing = self
                        .background_agents
                        .iter()
                        .find(|agent| agent.agent_id == *agent_id);
                    if metadata.resource_ref.is_some()
                        || existing.is_none_or(|agent| agent.resource_ref.is_none())
                    {
                        self.apply_current_event(
                            seq,
                            event,
                            kind,
                            metadata.native_origin,
                            metadata.clear_after_seq,
                        )?;
                        if let Some(agent) = self.background_agent(agent_id) {
                            agent.resource_ref = metadata.resource_ref;
                        }
                        if metadata.resource_ref.is_some() {
                            self.sidebar_observation_seq = seq;
                            self.sidebar_observation_origin = metadata.native_origin;
                        }
                    }
                }
                Event::BackgroundAgentProgress { agent_id, .. }
                | Event::BackgroundAgentCompleted { agent_id, .. } => {
                    if self.background_agents.iter().any(|agent| {
                        agent.agent_id == *agent_id && agent.resource_ref == metadata.resource_ref
                    }) {
                        self.apply_current_event(
                            seq,
                            event,
                            kind,
                            metadata.native_origin,
                            metadata.clear_after_seq,
                        )?;
                        if metadata.resource_ref.is_some() {
                            self.sidebar_observation_seq = seq;
                            self.sidebar_observation_origin = metadata.native_origin;
                        }
                    }
                }
                Event::BackgroundTaskStarted { task_id, .. } => {
                    let existing = self
                        .background_tasks
                        .iter()
                        .find(|task| task.task_id == *task_id);
                    if metadata.resource_ref.is_some()
                        || existing.is_none_or(|task| task.resource_ref.is_none())
                    {
                        self.apply_current_event(
                            seq,
                            event,
                            kind,
                            metadata.native_origin,
                            metadata.clear_after_seq,
                        )?;
                        if let Some(task) = self
                            .background_tasks
                            .iter_mut()
                            .find(|task| task.task_id == *task_id)
                        {
                            task.resource_ref = metadata.resource_ref;
                        }
                        if metadata.resource_ref.is_some() {
                            self.sidebar_observation_seq = seq;
                            self.sidebar_observation_origin = metadata.native_origin;
                        }
                    }
                }
                Event::BackgroundTaskFinished { task_id, .. }
                    if self.background_tasks.iter().any(|task| {
                        task.task_id == *task_id && task.resource_ref == metadata.resource_ref
                    }) =>
                {
                    self.apply_current_event(
                        seq,
                        event,
                        kind,
                        metadata.native_origin,
                        metadata.clear_after_seq,
                    )?;
                    if metadata.resource_ref.is_some() {
                        self.sidebar_observation_seq = seq;
                        self.sidebar_observation_origin = metadata.native_origin;
                    }
                }
                _ => {}
            }
        }
        self.last_seq = seq;
        self.updated_at = Utc::now();
        Ok(seq)
    }

    fn apply_current_event(
        &mut self,
        seq: u64,
        event: &Event,
        kind: crate::acp::operation_effect::ControlKind<'_>,
        origin: Option<NativeEventOrigin>,
        clear_after_seq: Option<u64>,
    ) -> Result<u64, StateError> {
        let previous_sidebar_phase = self.sidebar_phase;
        let closes_native = match &event {
            Event::AcpConnectionUnavailable { .. } => event.is_channel_loss(),
            Event::AgentStartupError {
                native_subject: Some(subject),
                ..
            } => matches!(subject.kind, NativeSubjectEvent::ChannelUnavailable(_)),
            _ => false,
        };
        if matches!(
            &event,
            Event::UserPromptSent { .. }
                | Event::UserDiffCommentsPrompt { .. }
                | Event::Stopped { .. }
                | Event::PromptRuntimeError { .. }
                | Event::SessionContextReset { .. }
                | Event::SessionCleared
                | Event::AgentSwitched { .. }
        ) {
            self.native_control_seq = seq;
        }
        match event {
            Event::PlanUpdated { plan } => {
                self.current_plan = Some(plan.clone());
                self.current_plan_seq = Some(seq);
            }
            Event::TodoListUpdated { todos } => self.todos = todos.clone(),
            Event::ToolCallStarted { tool_call } => {
                if !matches!(
                    self.sidebar_phase,
                    SidebarPhase::Error | SidebarPhase::PromptFailed
                ) {
                    self.sidebar_phase = SidebarPhase::Active;
                }
                self.start_tool_call(tool_call.clone())
            }
            Event::ToolCallCompleted { tool_call_id, .. } => {
                if self
                    .in_flight_tool
                    .as_ref()
                    .is_some_and(|t| t.id == *tool_call_id)
                {
                    self.in_flight_tool = None;
                }
                if self.compaction_tool_id.as_ref() == Some(tool_call_id) {
                    self.compaction_tool_id = None;
                    self.compacting = false;
                }
            }
            Event::ToolCallUpdated {
                tool_call_id,
                title,
                args_preview,
                started_at,
                diffs,
            } => {
                if let Some(tool) = self
                    .in_flight_tool
                    .as_mut()
                    .filter(|t| t.id == *tool_call_id)
                {
                    if let Some(title) = title {
                        tool.name = title.clone();
                    }
                    if let Some(args_preview) = args_preview {
                        tool.args_preview = args_preview.clone();
                    }
                    if let Some(started_at) = started_at {
                        tool.started_at = *started_at;
                    }
                    if let Some(diffs) = diffs {
                        tool.diffs = diffs.clone();
                    }
                }
            }
            Event::ApprovalRequested { approval } => {
                if !matches!(
                    self.sidebar_phase,
                    SidebarPhase::Error | SidebarPhase::PromptFailed
                ) {
                    self.sidebar_phase = SidebarPhase::Waiting;
                }
                self.pending_approvals.push(approval.clone())
            }
            Event::ApprovalResolved { nonce, .. } => {
                let pos = self
                    .pending_approvals
                    .iter()
                    .position(|a| a.nonce == *nonce)
                    .ok_or_else(|| StateError::UnknownApprovalNonce(nonce.clone()))?;
                if self.pending_approvals.remove(pos).resolved.is_some() {
                    return Err(StateError::ApprovalAlreadyResolved(nonce.clone()));
                }
                if !matches!(
                    self.sidebar_phase,
                    SidebarPhase::Error | SidebarPhase::PromptFailed
                ) {
                    self.sidebar_phase = SidebarPhase::Active;
                }
            }
            Event::ElicitationRequested { elicitation } => {
                // The asking tool call is now waiting on the user, not running.
                if elicitation.tool_call_id.is_some()
                    && self.in_flight_tool.as_ref().map(|t| &t.id)
                        == elicitation.tool_call_id.as_ref()
                {
                    self.in_flight_tool = None;
                }
                if !matches!(
                    self.sidebar_phase,
                    SidebarPhase::Error | SidebarPhase::PromptFailed
                ) {
                    self.sidebar_phase = SidebarPhase::Waiting;
                }
                self.pending_elicitations.push(elicitation.clone())
            }
            Event::ElicitationResolved { nonce, .. } => {
                self.pending_elicitations.retain(|e| e.nonce != *nonce);
                if !matches!(
                    self.sidebar_phase,
                    SidebarPhase::Error | SidebarPhase::PromptFailed
                ) {
                    self.sidebar_phase = SidebarPhase::Active;
                }
            }
            Event::DiffEmitted { diff } => {
                self.recent_diffs.push(diff.clone());
                let excess = self
                    .recent_diffs
                    .len()
                    .saturating_sub(Self::MAX_RECENT_DIFFS);
                self.recent_diffs.drain(..excess);
            }
            Event::ThinkingStarted => {
                self.thinking = Some(ThinkingSignal {
                    started_at: Utc::now(),
                });
                if self.sidebar_phase != SidebarPhase::PromptFailed {
                    self.turn_active = true;
                    self.sidebar_phase = SidebarPhase::Active;
                }
            }
            Event::ThinkingEnded => self.thinking = None,
            Event::RateLimit { info } => self.rate_limit = Some(info.clone()),
            Event::SessionNotice {
                severity,
                title,
                description,
                key,
            } => {
                let notice = SessionNotice {
                    id: notice_id(key.as_deref(), seq),
                    severity: severity.clone(),
                    title: title.clone(),
                    description: description.clone(),
                };
                match self.session_notices.iter_mut().find(|n| n.id == notice.id) {
                    Some(existing) => *existing = notice,
                    None => self.session_notices.push(notice),
                }
                let excess = self
                    .session_notices
                    .len()
                    .saturating_sub(Self::MAX_SESSION_NOTICES);
                self.session_notices.drain(..excess);
            }
            Event::UsageUpdated { usage } => self.usage = Some(usage.clone()),
            Event::ModeChanged { mode } => self.mode = *mode,
            Event::ModesAvailable {
                current_mode_id,
                modes,
            } => {
                self.available_modes = modes.clone();
                self.current_mode_id = Some(current_mode_id.clone());
            }
            Event::CurrentModeChanged { current_mode_id } => {
                self.current_mode_id = Some(current_mode_id.clone())
            }
            Event::AuthStatusUpdated { status } => self.auth_status = status.clone(),
            Event::AvailableCommandsUpdated { commands } => {
                self.available_commands = commands.clone()
            }
            Event::ConfigOptionsUpdated { options } => {
                // The failed value is now current, so the notice is moot.
                let confirmed = self.config_option_switch_failed.as_ref().is_some_and(|f| {
                    options
                        .iter()
                        .find(|opt| opt.id == f.config_id)
                        .is_some_and(|opt| opt.current_value == f.value)
                });
                if confirmed {
                    self.config_option_switch_failed = None;
                }
                self.config_options = options.clone();
            }
            Event::ConfigOptionSwitchFailed {
                config_id,
                value,
                reason,
            } => {
                self.config_option_switch_failed = Some(ConfigOptionSwitchFailure {
                    config_id: config_id.clone(),
                    value: value.clone(),
                    reason: reason.clone(),
                });
            }
            Event::PromptRuntimeError {
                message, evidence, ..
            } => {
                self.operation_diagnostic =
                    Some(crate::acp::acp_client::public_diagnostic(message));
                self.operation_error = *evidence;
                self.operation_diagnostic_origin = origin;
                self.end_turn("prompt_failed");
                self.sidebar_phase = SidebarPhase::Error;
            }
            Event::AcpConnectionUnavailable {
                source,
                cause,
                phase,
                native_subject,
            } => {
                let unavailable = AcpConnectionUnavailability {
                    source: *source,
                    cause: *cause,
                    phase: *phase,
                };
                self.native_availability = native_subject.map(|subject| (seq, subject));
                self.connection_unavailable = Some(unavailable);
                self.connection_ready = false;
            }
            Event::AgentStartupError {
                message,
                native_subject: Some(subject),
                ..
            } => {
                self.startup_diagnostic = Some(crate::acp::acp_client::public_diagnostic(message));
                match subject.kind {
                    NativeSubjectEvent::ChannelUnavailable(unavailable) => {
                        self.connection_unavailable = Some(unavailable);
                        self.native_availability = Some((seq, *subject));
                    }
                    NativeSubjectEvent::AdmissionRefusal(refusal) => {
                        let unavailable = refusal.observation();
                        self.connection_unavailable = Some(unavailable);
                        self.native_availability = Some((seq, *subject));
                    }
                    NativeSubjectEvent::ChannelReady => {
                        self.native_availability = Some((seq, *subject));
                    }
                    NativeSubjectEvent::PromptDiagnostic => {
                        self.turn_active = false;
                        self.cancelling = false;
                        self.sidebar_phase = SidebarPhase::PromptFailed;
                    }
                    NativeSubjectEvent::TurnEnded => {
                        self.end_turn("aborted");
                        self.native_control_seq = seq;
                    }
                    NativeSubjectEvent::StartupDiagnostic => {
                        self.turn_active = false;
                        self.cancelling = false;
                        self.sidebar_phase = SidebarPhase::Error;
                    }
                    NativeSubjectEvent::BindingObserved
                    | NativeSubjectEvent::RuntimeObservation => {}
                }
                self.connection_ready = false;
            }
            Event::AgentStartupError { message, .. } => {
                self.startup_diagnostic = Some(crate::acp::acp_client::public_diagnostic(message));
                self.turn_active = false;
                self.cancelling = false;
                self.sidebar_phase = SidebarPhase::Error;
            }
            Event::CancelRequested { .. } => self.cancelling = true,
            Event::Stopped { reason } => self.end_turn(reason),
            Event::IncompatibleAgent { detail } => {
                self.startup_error = Some(detail.clone());
                self.sidebar_phase = SidebarPhase::Error;
            }
            Event::UserPromptSent { .. } | Event::UserDiffCommentsPrompt { .. } => self.open_turn(),
            Event::PromptCapabilities { steering, .. } => self.steering = *steering,
            Event::AcpSessionAssigned {
                connection_ready,
                native_subject,
                ..
            } => {
                if *connection_ready
                    && native_subject
                        .is_some_and(|subject| subject.kind == NativeSubjectEvent::ChannelReady)
                {
                    self.connection_unavailable = None;
                    self.native_availability = native_subject.map(|subject| (seq, subject));
                    self.startup_error = None;
                    self.startup_diagnostic = None;
                    if self
                        .operation_diagnostic_origin
                        .zip(origin)
                        .is_some_and(|(failed, current)| failed != current)
                    {
                        // A different qualified Native establishment supersedes the failed operation.
                        // Reopening the same actor or replaying its Ready event does not.
                        self.operation_error = None;
                        self.operation_diagnostic = None;
                        self.operation_diagnostic_origin = None;
                        self.operation_output_uncertain = false;
                        self.recovery = None;
                    }
                    self.rate_limit = None;
                    self.heal_sidebar_phase();
                }
            }
            Event::RateLimitAutoResumed { .. } => self.heal_sidebar_phase(),
            Event::SessionContextReset { .. } => {
                self.usage = None;
                self.operation_error = None;
                self.operation_diagnostic = None;
                self.operation_diagnostic_origin = None;
                self.operation_output_uncertain = false;
                self.recovery = None;
            }
            // Clears the conversation; adapter-scoped commands, modes and
            // selectors are never re-announced, so they stay.
            Event::SessionCleared => {
                self.usage = None;
                self.operation_error = None;
                self.operation_diagnostic = None;
                self.operation_diagnostic_origin = None;
                self.operation_output_uncertain = false;
                self.recovery = None;
                if self.sidebar_phase == SidebarPhase::PromptFailed {
                    self.sidebar_phase = SidebarPhase::Idle;
                }
                if clear_after_seq
                    .is_none_or(|cut| self.current_plan_seq.is_none_or(|seq| seq <= cut))
                {
                    self.current_plan = None;
                    self.current_plan_seq = None;
                }
                self.mode = SessionMode::Default;
                self.pending_approvals = Vec::new();
                self.pending_elicitations = Vec::new();
            }
            Event::ConversationCompactionStarted => self.compacting = true,
            Event::ConversationCompacted => {
                self.compacting = false;
                self.usage = None;
            }
            Event::ConversationCompactionFailed => {
                self.compacting = false;
                self.compaction_tool_id = None;
            }
            Event::PromptRejected { .. } => self.turn_active = false,
            Event::AgentSwitched { from, to, reason } => {
                self.switch_agent(from.clone(), to.clone(), reason.clone())
            }
            Event::BackgroundAgentLaunched {
                agent_id,
                tool_call_id,
                description,
                prompt,
                model,
                output_file: _,
                workflow,
                started_at,
            } => self.launch_background_agent(BackgroundAgentRecord {
                agent_id: agent_id.clone(),
                resource_ref: None,
                tool_call_id: tool_call_id.clone(),
                description: description.clone(),
                prompt: prompt.clone(),
                model: model.clone(),
                workflow: workflow.clone(),
                status: BackgroundAgentStatus::Running,
                started_at: *started_at,
                ended_at: None,
                tool_count: 0,
                tools: Vec::new(),
                last_tool: None,
                last_text: None,
                result: None,
                warning: None,
            }),
            Event::BackgroundAgentProgress {
                agent_id,
                status,
                tool_count,
                tools,
                last_tool,
                last_text,
                ..
            } => {
                // Stalled is non-terminal, but ended records must never reopen.
                if let Some(a) = self
                    .background_agent(agent_id)
                    .filter(|a| a.ended_at.is_none() && !a.status.is_terminal())
                {
                    a.status = *status;
                    a.tool_count = *tool_count;
                    replace_if_some(&mut a.last_tool, last_tool.clone());
                    replace_if_some(&mut a.last_text, last_text.clone());
                    if !tools.is_empty() {
                        a.tools = tools.clone();
                    }
                }
            }
            Event::BackgroundAgentCompleted {
                agent_id,
                status,
                tools,
                result,
                warning,
                ended_at,
            } => {
                if let Some(a) = self.background_agent(agent_id) {
                    a.status = *status;
                    a.ended_at = Some(*ended_at);
                    replace_if_some(&mut a.result, result.clone());
                    replace_if_some(&mut a.warning, warning.clone());
                    if !tools.is_empty() {
                        a.tools = tools.clone();
                    }
                }
            }
            Event::BackgroundTaskStarted {
                task_id,
                tool_call_id,
                workflow,
                ..
            } => {
                // A reused task id replaces whatever run held it.
                self.close_workflow_run(task_id, None);
                if let (Some(workflow), Some(tool_call_id)) = (workflow, tool_call_id) {
                    self.workflow_runs.push(WorkflowRunRecord {
                        task_id: task_id.clone(),
                        tool_call_id: tool_call_id.clone(),
                        name: workflow.name.clone(),
                        summary: workflow.summary.clone(),
                        finished: false,
                        ended_at: None,
                    });
                }
                if !self
                    .background_tasks
                    .iter()
                    .any(|task| task.task_id == *task_id)
                {
                    self.background_tasks.push(BackgroundTaskRecord {
                        task_id: task_id.clone(),
                        resource_ref: None,
                    });
                }
            }
            Event::BackgroundTaskFinished { task_id, ended_at } => {
                self.close_workflow_run(task_id, *ended_at);
                self.background_tasks
                    .retain(|task| task.task_id != *task_id);
            }
            // Titles live on `Instance`; wakeups and monitors are read from the log.
            Event::AgentMessageChunk { .. } => {
                if self.sidebar_phase != SidebarPhase::PromptFailed {
                    self.sidebar_phase = SidebarPhase::Active;
                }
            }
            Event::SessionTitleSuggested { .. }
            | Event::ToolCallContent { .. }
            | Event::ModeSwitchFailed { .. }
            | Event::RawAgentUpdate { .. }
            | Event::ConversationSummary { .. }
            | Event::ConversationCompactionSummary { .. }
            | Event::WakeupScheduled { .. }
            | Event::MonitorArmed { .. } => {}
        }
        if closes_native {
            self.retire_unavailable_controls();
            self.sidebar_phase = SidebarPhase::Error;
        }
        use crate::acp::operation_effect::ControlKind;
        let passive_heal = matches!(
            event,
            Event::AcpSessionAssigned { .. } | Event::RateLimitAutoResumed { .. }
        );
        if matches!(
            kind,
            ControlKind::Cleared | ControlKind::ContextReset | ControlKind::Switched
        ) || (passive_heal && self.sidebar_phase != previous_sidebar_phase)
        {
            if self.sidebar_phase == SidebarPhase::Idle && !self.has_qualified_background_work() {
                self.sidebar_observation_seq = 0;
                self.sidebar_observation_origin = None;
            }
        } else if matches!(
            kind,
            ControlKind::Admission
                | ControlKind::Diagnostic
                | ControlKind::Stopped(_)
                | ControlKind::AgentWork
                | ControlKind::Unavailable { .. }
        ) || (self.sidebar_phase != previous_sidebar_phase && !passive_heal)
        {
            self.sidebar_observation_seq = seq;
            self.sidebar_observation_origin = origin;
        }
        self.last_seq = seq;
        self.updated_at = Utc::now();
        Ok(self.last_seq)
    }

    /// These are actionable callback metadata, not transcript/history or a
    /// fabricated permission decision. Ordinary request failure never calls this.
    fn retire_unavailable_controls(&mut self) {
        self.turn_active = false;
        self.cancelling = false;
        self.compacting = false;
        self.compaction_tool_id = None;
        self.in_flight_tool = None;
        self.thinking = None;
        self.pending_approvals.clear();
        self.pending_elicitations.clear();
        self.original_decision_nonces.clear();
    }

    fn start_tool_call(&mut self, tool_call: ToolCall) {
        if tool_call.name == COMPACT_TOOL_NAME {
            self.compacting = true;
            self.compaction_tool_id = Some(tool_call.id.clone());
        }
        match self.in_flight_tool.as_mut() {
            // A repeated start frame for the same call keeps diffs an update already attached.
            Some(existing) if existing.id == tool_call.id => {
                let diffs = std::mem::take(&mut existing.diffs);
                *existing = tool_call;
                if existing.diffs.is_empty() {
                    existing.diffs = diffs;
                }
            }
            _ => self.in_flight_tool = Some(tool_call),
        }
        self.thinking = None;
    }

    /// A prompt opens a turn (or steers the running one) and ends any rate-limit park.
    fn open_turn(&mut self) {
        let steered = self.turn_active && self.steering;
        self.turn_active = true;
        self.sidebar_phase = SidebarPhase::Active;
        self.operation_error = None;
        self.operation_diagnostic = None;
        self.operation_diagnostic_origin = None;
        self.operation_output_uncertain = false;
        self.recovery = None;
        if !steered {
            self.cancelling = false;
        }
        self.rate_limit = None;
        // Advisories describe the turn they arrived in, so a new prompt retires
        // them. Their transcript rows keep the history.
        self.session_notices.clear();
    }

    /// A fresh worker attached: a startup error no longer holds the row.
    fn heal_sidebar_phase(&mut self) {
        if self.sidebar_phase == SidebarPhase::Error
            && self.operation_error.is_none()
            && self.operation_diagnostic.is_none()
        {
            self.sidebar_phase = SidebarPhase::Idle;
        }
    }

    /// The turn is over however it ended, so every in-turn phase clears.
    fn end_turn(&mut self, reason: &str) {
        self.turn_active = false;
        self.sidebar_phase = if reason == "prompt_failed" || reason == "aborted" {
            SidebarPhase::PromptFailed
        } else if reason == "session_reset_failed" {
            SidebarPhase::Error
        } else {
            SidebarPhase::Idle
        };
        self.cancelling = false;
        self.compacting = false;
        self.compaction_tool_id = None;
        self.in_flight_tool = None;
        self.thinking = None;
        if reason != "rate_limited" && reason != RATE_LIMIT_EXHAUSTED_RETRIES_REASON {
            self.rate_limit = None;
        }
    }

    /// The new backend knows nothing of the prior agent's session or capabilities.
    fn switch_agent(&mut self, from: String, to: String, reason: String) {
        self.agent = AgentName(to.clone());
        self.rate_limit = None;
        self.operation_error = None;
        self.operation_diagnostic = None;
        self.operation_diagnostic_origin = None;
        self.operation_output_uncertain = false;
        self.recovery = None;
        if self.sidebar_phase == SidebarPhase::PromptFailed {
            self.sidebar_phase = SidebarPhase::Idle;
        }
        // The prior agent's advisories say nothing about the new one.
        self.session_notices.clear();
        self.in_flight_tool = None;
        self.thinking = None;
        self.pending_approvals = Vec::new();
        self.pending_elicitations = Vec::new();
        self.usage = None;
        self.available_commands = Vec::new();
        self.available_modes = Vec::new();
        self.current_mode_id = None;
        self.current_plan = None;
        self.current_plan_seq = None;
        self.original_decision_nonces.clear();
        self.mode = SessionMode::Default;
        self.config_options = Vec::new();
        self.config_option_switch_failed = None;
        self.last_agent_switch = Some(AgentSwitchInfo {
            from,
            to,
            reason,
            switched_at: Utc::now(),
        });
    }

    /// Idempotent on replay: a relaunch replaces the record for the same agent id.
    fn launch_background_agent(&mut self, record: BackgroundAgentRecord) {
        match self.background_agent(&record.agent_id) {
            Some(existing) => *existing = record,
            None => self.background_agents.push(record),
        }
    }

    fn background_agent(&mut self, agent_id: &str) -> Option<&mut BackgroundAgentRecord> {
        self.background_agents
            .iter_mut()
            .find(|a| a.agent_id == agent_id)
    }
}

fn replace_if_some<T>(slot: &mut Option<T>, value: Option<T>) {
    if value.is_some() {
        *slot = value;
    }
}

/// Event fixtures shared by every module that folds the log.
#[cfg(test)]
pub(crate) mod test_support {
    use super::Event;

    pub(crate) fn prompt(text: &str) -> Event {
        Event::UserPromptSent {
            prompt_id: None,
            text: text.into(),
            attachments: Vec::new(),
            synthesized: false,
            origin_plugin: None,
        }
    }

    pub(crate) fn chunk(text: &str) -> Event {
        Event::AgentMessageChunk {
            text: text.into(),
            message_id: None,
        }
    }

    pub(crate) fn stopped(reason: &str) -> Event {
        Event::Stopped {
            reason: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{prompt, stopped};
    use super::*;

    fn fresh_state() -> AcpState {
        AcpState::new(
            AcpSessionId("s-1".into()),
            AgentName("aoe-agent".into()),
            Some("claude-opus-4-7".into()),
        )
    }

    fn applied(events: impl IntoIterator<Item = Event>) -> AcpState {
        let mut s = fresh_state();
        for event in events {
            s.apply_scoped_event(
                s.last_seq.saturating_add(1),
                &event,
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        }
        s
    }

    fn current_session_metadata() -> crate::acp::event_sender::EventMetadata {
        crate::acp::event_sender::EventMetadata {
            scope: crate::acp::event_sender::EventScope::SessionCurrent,
            ..crate::acp::event_sender::EventMetadata::history()
        }
    }

    // Model qualified current session events; background resources remain historical.
    fn applied_current_session(events: impl IntoIterator<Item = Event>) -> AcpState {
        let mut state = fresh_state();
        for event in events {
            let metadata = match &event {
                Event::BackgroundAgentLaunched { .. }
                | Event::BackgroundAgentProgress { .. }
                | Event::BackgroundAgentCompleted { .. }
                | Event::BackgroundTaskStarted { .. }
                | Event::BackgroundTaskFinished { .. } => {
                    crate::acp::event_sender::EventMetadata::history()
                }
                _ => current_session_metadata(),
            };
            state
                .apply_scoped_event(state.last_seq + 1, &event, &metadata)
                .unwrap();
        }
        state
    }

    #[test]
    fn historical_session_reports_do_not_grant_current_control_state() {
        let state = applied([
            Event::AuthStatusUpdated {
                status: Some(auth(AuthStatusKind::Account, "historical account")),
            },
            config_options("claude-sonnet-4-6"),
            caps(true),
            prompt("historical request"),
            Event::ThinkingStarted,
            Event::ToolCallStarted {
                tool_call: tool_call("historical-tool"),
            },
            rate_limit(),
            observed_channel_ready("historical-channel"),
        ]);
        assert!(state.auth_status.is_none());
        assert!(state.config_options.is_empty());
        assert!(!state.steering && !state.turn_active);
        assert!(state.thinking.is_none() && state.in_flight_tool.is_none());
        assert!(state.rate_limit.is_none());
        assert!(!state.connection_ready && state.native_availability.is_none());
        assert_eq!(state.sidebar_status(), Status::Idle);
    }

    fn observed_channel_ready(acp_session_id: &str) -> Event {
        Event::AcpSessionAssigned {
            acp_session_id: acp_session_id.into(),
            connection_ready: true,
            native_subject: Some(NativeEventSubject {
                context: NativeEventContext([0xa5; 32]),
                owner: None,
                kind: NativeSubjectEvent::ChannelReady,
            }),
        }
    }

    #[test]
    fn failed_operation_metadata_replays_without_erasing_owned_state() {
        use crate::session::{
            ErrorEvidenceSource, ErrorOperation, OperationErrorEvidence, RuntimeEvidence,
        };
        let evidence = OperationErrorEvidence::new(
            ErrorEvidenceSource::LocalPromptResult,
            ErrorOperation::Prompt,
        );
        let mut state = applied([prompt("work"), super::test_support::chunk("partial output")]);
        let correlation = crate::acp::establishment::NativeOperationCorrelation::new(None);
        let mut qualification = crate::acp::event_sender::EventMetadata::history();
        qualification.operation = Some(crate::acp::protocol::AcpOperationMetadata {
            correlation,
            outcome: None,
        });
        let mut admission = qualification.clone();
        admission.scope = crate::acp::event_sender::EventScope::OperationCurrent;
        state
            .apply_scoped_event(state.last_seq + 1, &Event::ThinkingStarted, &admission)
            .unwrap();
        state.cancelling = true;
        state.compacting = true;
        state.compaction_tool_id = Some("partial-tool".into());
        state.in_flight_tool = Some(tool_call("partial-tool"));
        let mut unrelated = qualification.clone();
        unrelated.operation.as_mut().unwrap().correlation =
            crate::acp::establishment::NativeOperationCorrelation::new(None);
        state
            .apply_scoped_event(
                state.last_seq + 1,
                &Event::PromptRuntimeError {
                    message: "older request failed".into(),
                    evidence: None,
                    native_subject: None,
                },
                &unrelated,
            )
            .unwrap();
        assert!(state.turn_active && state.thinking.is_some() && state.compacting);
        assert_eq!(state.operation_effects.active(), Some(correlation));
        state.usage = Some(SessionUsage {
            used: 12,
            size: 100,
            cost: None,
        });
        let approval = crate::acp::permissions::build_approval(
            ToolCall {
                id: "pending".into(),
                name: "shell".into(),
                kind: "execute".into(),
                args_preview: "{}".into(),
                started_at: Utc::now(),
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: Vec::new(),
            },
            Vec::new(),
        );
        state.pending_approvals.push(approval);
        state.pending_elicitations.push(Elicitation {
            nonce: Nonce("pending-question".into()),
            message: "Choose".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions: Vec::new(),
            requested_at: Utc::now(),
            resolved: None,
        });
        for metadata in [Some(evidence), None] {
            let mut replay = state.clone();
            let event = Event::PromptRuntimeError {
                message: "request failed".into(),
                evidence: metadata,
                native_subject: None,
            };
            let decoded: Event =
                serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
            replay
                .apply_scoped_event(replay.last_seq + 1, &decoded, &qualification)
                .unwrap();
            assert!(replay.thinking.is_none() && replay.in_flight_tool.is_none());
            assert!(!replay.cancelling && !replay.compacting);
            assert!(
                replay.compaction_tool_id.is_none() && replay.operation_effects.active().is_none()
            );
            replay
                .apply_scoped_event(
                    replay.last_seq + 1,
                    &stopped("prompt_failed"),
                    &qualification,
                )
                .unwrap();
            replay
                .apply_scoped_event(
                    replay.last_seq + 1,
                    &Event::ThinkingStarted,
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
            replay
                .apply_scoped_event(
                    replay.last_seq + 1,
                    &super::test_support::chunk("late native output"),
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
            replay
                .apply_scoped_event(
                    replay.last_seq + 1,
                    &Event::BackgroundTaskStarted {
                        task_id: "owned-background-work".into(),
                        tool_call_id: Some("background-tool".into()),
                        description: "background activity remains observed".into(),
                        workflow: None,
                    },
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
            assert!(
                replay.has_active_background_agent(),
                "background observations survive the failed main turn"
            );
            assert!(!replay.turn_active);
            assert_eq!(replay.sidebar_status(), Status::Error);
            assert_eq!(replay.operation_error, metadata);
            assert_eq!(replay.pending_approvals.len(), 1);
            assert_eq!(replay.pending_elicitations.len(), 1);
            assert_eq!(
                serde_json::to_value(&replay.usage).unwrap(),
                serde_json::to_value(&state.usage).unwrap()
            );
            assert!(replay.startup_error.is_none());
            if let Some(evidence) = replay.operation_error {
                assert_eq!(evidence.runtime, RuntimeEvidence::Unknown);
                assert_eq!(evidence.retryable, None);
            }
            replay
                .apply_scoped_event(
                    replay.last_seq + 1,
                    &Event::AcpSessionAssigned {
                        acp_session_id: "same-native-history".into(),
                        connection_ready: false,
                        native_subject: None,
                    },
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
            assert_eq!(
                replay.sidebar_status(),
                Status::Error,
                "reattach is not a new successful operation"
            );
            assert_eq!(replay.operation_error, metadata);
            replay
                .apply_scoped_event(
                    replay.last_seq + 1,
                    &prompt("next explicit request"),
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
            assert!(
                !replay.turn_active,
                "host history is not another admitted operation"
            );
            let stale_failure_seq = replay.last_seq;
            let mut history_only = replay.clone();
            history_only.apply_native_availability(NativeAvailabilityProjection {
                control_failure: Some((stale_failure_seq, NativeControlFailure::Prompt)),
                ..NativeAvailabilityProjection::default()
            });
            assert!(
                !history_only.turn_active && !history_only.connection_ready,
                "host history cannot retire the failure or establish channel readiness"
            );
            let mut next = admission.clone();
            let next_correlation = crate::acp::establishment::NativeOperationCorrelation::new(None);
            next.operation.as_mut().unwrap().correlation = next_correlation;
            replay
                .apply_scoped_event(replay.last_seq + 1, &Event::ThinkingStarted, &next)
                .unwrap();
            replay.apply_native_availability(NativeAvailabilityProjection {
                control_failure: Some((stale_failure_seq, NativeControlFailure::Prompt)),
                ..NativeAvailabilityProjection::default()
            });
            assert!(
                replay.turn_active && replay.thinking.is_some(),
                "a newer admitted operation supersedes an older failed-append hint"
            );
            replay
                .apply_scoped_event(replay.last_seq + 1, &decoded, &qualification)
                .unwrap();
            replay
                .apply_scoped_event(
                    replay.last_seq + 1,
                    &stopped("prompt_failed"),
                    &qualification,
                )
                .unwrap();
            assert!(
                replay.turn_active && replay.thinking.is_some() && replay.operation_error.is_none()
            );
            assert_eq!(replay.operation_effects.active(), Some(next_correlation));
        }
        let mut refused = state.clone();
        let management = AcpConnectionUnavailability {
            source: AcpConnectionSource::RunnerRecord,
            cause: AcpConnectionCause::ControlProtocolUnknown,
            phase: AcpConnectionPhase::Establishing,
        };
        refused
            .apply_scoped_event(
                refused.last_seq + 1,
                &Event::AcpConnectionUnavailable {
                    source: management.source,
                    cause: management.cause,
                    phase: management.phase,
                    native_subject: None,
                },
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        refused.apply_native_availability(NativeAvailabilityProjection {
            unavailable: Some(management),
            ..NativeAvailabilityProjection::default()
        });
        assert!(
            refused.turn_active,
            "management refusal does not finish retained native work"
        );
        assert!(!refused.connection_ready);
        assert_eq!(refused.sidebar_status(), Status::Error);
        assert_eq!(refused.pending_approvals.len(), 1);
        assert_eq!(refused.pending_elicitations.len(), 1);
        let mut unavailable = crate::acp::event_sender::EventMetadata::history();
        unavailable.scope = crate::acp::event_sender::EventScope::SessionCurrent;
        unavailable.availability_denial = true;
        refused
            .apply_scoped_event(
                refused.last_seq + 1,
                &Event::AcpConnectionUnavailable {
                    source: AcpConnectionSource::Native,
                    cause: AcpConnectionCause::StdoutEof,
                    phase: AcpConnectionPhase::Established,
                    native_subject: None,
                },
                &unavailable,
            )
            .unwrap();
        assert!(!refused.turn_active);
        assert!(refused.pending_approvals.is_empty());
        assert!(refused.pending_elicitations.is_empty());
        for json in [
            r#"{"PromptRuntimeError":{"message":"old failure"}}"#,
            r#"{"PromptRuntimeError":{"message":"old failure","evidence":null}}"#,
        ] {
            assert!(matches!(
                serde_json::from_str::<Event>(json).unwrap(),
                Event::PromptRuntimeError { evidence: None, .. }
            ));
        }
    }

    #[test]
    fn optional_native_receipts_and_private_subjects_do_not_create_replay_authority() {
        for json in [
            r#"{"AcpSessionAssigned":{"acp_session_id":"legacy"}}"#,
            r#"{"AcpSessionAssigned":{"acp_session_id":"legacy","connection_ready":null}}"#,
        ] {
            assert!(matches!(
                serde_json::from_str::<Event>(json).unwrap(),
                Event::AcpSessionAssigned {
                    connection_ready: false,
                    native_subject: None,
                    ..
                }
            ));
        }
        for absent in [true, false] {
            let mut json = serde_json::to_value(fresh_state()).unwrap();
            if absent {
                json.as_object_mut().unwrap().remove("connection_ready");
            } else {
                json["connection_ready"] = serde_json::Value::Null;
            }
            let decoded: AcpState = serde_json::from_value(json).unwrap();
            assert!(!decoded.connection_ready);
            assert!(decoded.native_availability.is_none());
        }
        let context = NativeEventContext([0xa5; 32]);
        let subject = NativeEventSubject {
            context,
            owner: None,
            kind: NativeSubjectEvent::ChannelReady,
        };
        let event = Event::AcpSessionAssigned {
            acp_session_id: "retained-conversation".into(),
            connection_ready: true,
            native_subject: Some(subject),
        };
        let mut folded = fresh_state();
        folded
            .apply_scoped_event(
                7,
                &event.clone(),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        let frame = crate::acp::protocol::AcpBroadcastFrame::from_qualified(
            "s-1".into(),
            7,
            std::sync::Arc::new(event.clone()),
            &crate::acp::event_sender::EventMetadata::history(),
            Some(99),
            Some(subject),
            Some(Status::Idle),
        );
        let public = serde_json::to_string(&serde_json::json!({
            "frames": [frame], "state": folded, "event": event,
        }))
        .unwrap();
        for private in [
            "native_subject",
            "native_availability",
            "native_control_seq",
            "sidebar_observation_seq",
            "sidebar_observation_origin",
            "birth_fingerprint",
            "worker_generation",
        ] {
            assert!(
                !public.contains(private),
                "{private} escaped into public JSON"
            );
        }
        let replayed: Event =
            serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert!(replayed.native_subject().is_none());
        let historical = NativeAvailabilityProjection::resolve(
            Some((7, subject)),
            Some(context),
            None,
            true,
            None,
        );
        assert!(
            !historical.ready,
            "a recorded assignment is not a current usable channel"
        );
    }

    #[test]
    fn native_loss_and_explicit_wake_eligibility_need_current_qualified_context() {
        let context = NativeEventContext([1; 32]);
        let unavailable = AcpConnectionUnavailability {
            source: AcpConnectionSource::Native,
            cause: AcpConnectionCause::StdoutEof,
            phase: AcpConnectionPhase::Established,
        };
        let subject = NativeEventSubject {
            context,
            owner: None,
            kind: NativeSubjectEvent::ChannelUnavailable(unavailable),
        };
        let current_loss = NativeAvailabilityProjection::resolve(
            Some((9, subject)),
            Some(context),
            None,
            true,
            None,
        )
        .with_prompt_wake_eligibility(true);
        assert_eq!(current_loss.unavailable, Some(unavailable));
        assert!(!current_loss.ready && !current_loss.prompt_wake_eligible);
        for (qualified, owner_current) in [
            (None, true),
            (Some(NativeEventContext([2; 32])), true),
            (Some(context), false),
        ] {
            let unknown = NativeAvailabilityProjection::resolve(
                Some((9, subject)),
                qualified,
                None,
                owner_current,
                None,
            );
            assert!(!unknown.ready && unknown.unavailable.is_none());
        }
        let mut sdk_subject = subject;
        if let NativeSubjectEvent::ChannelUnavailable(loss) = &mut sdk_subject.kind {
            loss.source = AcpConnectionSource::Sdk;
        }
        let old_sdk = NativeAvailabilityProjection::resolve(
            Some((9, sdk_subject)),
            Some(context),
            None,
            true,
            None,
        );
        assert!(
            old_sdk.unavailable.is_none(),
            "a previous SDK socket is not current native loss"
        );
        assert!(
            !NativeAvailabilityProjection::default()
                .with_prompt_wake_eligibility(true)
                .prompt_wake_eligible
        );
        let known_dormant =
            NativeAvailabilityProjection::resolve(None, Some(context), None, false, None)
                .with_prompt_wake_eligibility(true);
        assert!(known_dormant.prompt_wake_eligible && !known_dormant.ready);
        assert!(
            !NativeAvailabilityProjection::resolve(None, Some(context), None, false, None)
                .prompt_wake_eligible,
            "a coherent context alone does not admit a prompt intent"
        );
    }

    #[test]
    fn carried_native_receipts_recheck_loss_and_the_original_lifecycle_after_await() {
        use crate::acp::runner_lifecycle::{LifecycleTable, ResumeKind};
        use std::sync::{Arc, Mutex};
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let mut row = crate::session::Instance::new("receipt", "/tmp");
        row.id = "receipt".into();
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        let scope = NativeObservationScope::capture(&row, &storage).unwrap();
        let (client, _events) =
            crate::acp::acp_client::AcpClient::fake_for_test(AcpSessionId("receipt".into()));
        client.test_observe_native_ready();
        let client = Arc::new(client);
        let mut table = LifecycleTable::new(1);
        let lease = table.admit("receipt", ResumeKind::Spawn).unwrap();
        table.install(&lease, None).unwrap();
        let lifecycle = Arc::new(Mutex::new(table));
        let mut carried = CurrentNativeAvailability {
            scope,
            owner: None,
            ready: true,
            unavailable: None,
            client: Arc::clone(&client),
            lifecycle: Arc::clone(&lifecycle),
            lease: lease.clone(),
        };
        assert!(!carried.refresh("receipt", false).unwrap() && !carried.ready);
        assert!(carried.refresh("receipt", true).unwrap() && carried.ready);
        let held = lifecycle.lock().unwrap();
        assert!(
            carried.refresh("receipt", true).is_err(),
            "identified lifecycle contention is distinguishable from stable Unknown"
        );
        drop(held);
        client.test_observe_native_loss();
        assert!(carried.refresh("receipt", true).unwrap());
        assert!(!carried.ready);
        assert_eq!(
            carried.unavailable.unwrap().cause,
            AcpConnectionCause::StdoutEof
        );
        let mut table = lifecycle.lock().unwrap();
        assert!(table.release_running(&lease));
        let replacement = table.admit("receipt", ResumeKind::Spawn).unwrap();
        table.install(&replacement, None).unwrap();
        drop(table);
        assert!(
            !carried.refresh("receipt", true).unwrap(),
            "a replacement cannot reuse the captured client's receipt"
        );
    }

    /// The sidebar projection over event sequences, including the ones the old
    /// per-event table got right only by reading outside activity flags.
    #[test]
    fn sidebar_status_projects_the_folded_events() {
        use self::applied_current_session as applied;
        use super::test_support::chunk;
        use crate::acp::permissions::build_approval;
        let tool = || ToolCall {
            id: "t".into(),
            name: "shell".into(),
            kind: "execute".into(),
            args_preview: "{}".into(),
            started_at: Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        };
        let approval = build_approval(tool(), Vec::new());
        let nonce = approval.nonce.clone();
        let elicitation = Elicitation {
            nonce: Nonce("e-1".into()),
            message: "Pick".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions: Vec::new(),
            requested_at: Utc::now(),
            resolved: None,
        };
        let launched = || Event::BackgroundAgentLaunched {
            agent_id: "bg-1".into(),
            tool_call_id: "tc-1".into(),
            description: "map backend".into(),
            prompt: "do it".into(),
            model: "claude-opus-4-8".into(),
            output_file: "/tmp/bg-1.output".into(),
            started_at: Utc::now(),
            workflow: None,
        };
        let completed = |status| Event::BackgroundAgentCompleted {
            agent_id: "bg-1".into(),
            status,
            tools: Vec::new(),
            result: None,
            warning: None,
            ended_at: Utc::now(),
        };
        let startup_error = || Event::AgentStartupError {
            message: "boom".into(),
            native_subject: None,
        };
        let assigned = || observed_channel_ready("a-1");
        let cases: Vec<(&str, Vec<Event>, Status)> = vec![
            ("nothing yet", vec![], Status::Idle),
            ("open turn", vec![prompt("go")], Status::Running),
            (
                "turn ended",
                vec![prompt("go"), stopped("end_turn")],
                Status::Idle,
            ),
            // A turn resumed agent-side (fired wakeup, background output) streams
            // these without a UserPromptSent.
            ("chunk without a prompt", vec![chunk("x")], Status::Running),
            (
                "thinking without a prompt",
                vec![Event::ThinkingStarted],
                Status::Running,
            ),
            (
                "tool call without a prompt",
                vec![Event::ToolCallStarted { tool_call: tool() }],
                Status::Running,
            ),
            (
                "pending approval",
                vec![
                    prompt("go"),
                    Event::ApprovalRequested {
                        approval: approval.clone(),
                    },
                ],
                Status::Waiting,
            ),
            (
                "approval answered",
                vec![
                    prompt("go"),
                    Event::ApprovalRequested { approval },
                    Event::ApprovalResolved {
                        nonce,
                        decision: ApprovalDecision::Allow,
                    },
                ],
                Status::Running,
            ),
            (
                "pending elicitation",
                vec![
                    prompt("go"),
                    Event::ElicitationRequested {
                        elicitation: elicitation.clone(),
                    },
                ],
                Status::Waiting,
            ),
            (
                "elicitation answered",
                vec![
                    prompt("go"),
                    Event::ElicitationRequested { elicitation },
                    Event::ElicitationResolved {
                        nonce: Nonce("e-1".into()),
                        outcome: ElicitationOutcome::Cancelled,
                        answers: Vec::new(),
                    },
                ],
                Status::Running,
            ),
            // The stop ends the turn even though the request was never answered.
            (
                "stopped while blocked",
                vec![
                    prompt("go"),
                    Event::ApprovalRequested {
                        approval: build_approval(tool(), Vec::new()),
                    },
                    stopped("cancelled"),
                ],
                Status::Idle,
            ),
            // #4001: a detach closes the record like any terminal status.
            (
                "detached agent then stopped",
                vec![
                    prompt("go"),
                    launched(),
                    completed(BackgroundAgentStatus::Detached),
                    stopped("user_stopped"),
                ],
                Status::Idle,
            ),
            (
                "agent completes mid-turn",
                vec![
                    prompt("go"),
                    launched(),
                    completed(BackgroundAgentStatus::Completed),
                ],
                Status::Running,
            ),
            (
                "historical backgrounds cannot reopen a completed main turn",
                vec![
                    prompt("go"),
                    launched(),
                    Event::BackgroundTaskStarted {
                        task_id: "task-1".into(),
                        tool_call_id: None,
                        description: "sleep".into(),
                        workflow: None,
                    },
                    stopped("end_turn"),
                    launched(),
                ],
                Status::Idle,
            ),
            (
                "startup error",
                vec![prompt("go"), startup_error()],
                Status::Error,
            ),
            (
                "startup error then a new prompt",
                vec![startup_error(), prompt("retry")],
                Status::Running,
            ),
            (
                "worker reattached",
                vec![startup_error(), assigned()],
                Status::Idle,
            ),
            (
                "binding-only observation does not heal a startup error",
                vec![
                    startup_error(),
                    Event::AcpSessionAssigned {
                        acp_session_id: "binding-only".into(),
                        connection_ready: false,
                        native_subject: None,
                    },
                ],
                Status::Error,
            ),
            (
                "bare ready flag does not heal a startup error",
                vec![
                    startup_error(),
                    Event::AcpSessionAssigned {
                        acp_session_id: "unqualified".into(),
                        connection_ready: true,
                        native_subject: None,
                    },
                ],
                Status::Error,
            ),
            (
                "assignment mid-turn",
                vec![prompt("go"), assigned()],
                Status::Running,
            ),
            (
                "incompatible agent",
                vec![Event::IncompatibleAgent {
                    detail: StartupErrorDetail::MissingAgentInfo {
                        expected_package: "pkg".into(),
                        install_command: "install".into(),
                        auto_install: false,
                    },
                }],
                Status::Error,
            ),
        ];
        for (name, events, want) in cases {
            let state = applied(events);
            assert_eq!(state.sidebar_status(), want, "{name}");
            if name == "historical backgrounds cannot reopen a completed main turn" {
                assert!(!state.turn_active);
                assert!(state.has_active_background_agent());
                assert_eq!(state.background_agents.len(), 1);
                assert_eq!(state.background_tasks.len(), 1);
                assert!(state.operation_effects.active().is_none());
            }
        }
    }

    fn notice(title: &str) -> Event {
        Event::SessionNotice {
            severity: "warning".into(),
            title: title.into(),
            description: None,
            key: None,
        }
    }

    #[test]
    fn keyed_session_notice_replaces_in_place() {
        let keyed = |description: &str| Event::SessionNotice {
            severity: "error".into(),
            title: "Compaction failed".into(),
            description: Some(description.into()),
            key: Some("compaction-a".into()),
        };
        let s = applied([keyed("aborted"), notice("other"), keyed("out of tokens")]);
        let got: Vec<(&str, Option<&str>)> = s
            .session_notices
            .iter()
            .map(|n| (n.id.as_str(), n.description.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("notice-compaction-a", Some("out of tokens")),
                ("notice-2", None)
            ]
        );
    }

    /// #4242: notices are live advisories, so they are capped and retired on
    /// the next turn rather than accumulating for the life of the session.
    #[test]
    fn session_notices_are_capped_and_retired_by_the_next_turn() {
        use self::applied_current_session as applied;
        let titles = |s: &AcpState| -> Vec<String> {
            s.session_notices.iter().map(|n| n.title.clone()).collect()
        };

        let one = applied([notice("first")]);
        assert_eq!(titles(&one), ["first"]);
        assert_eq!(one.session_notices[0].id, "notice-1", "id keys dismissal");

        let overflowed =
            applied((0..AcpState::MAX_SESSION_NOTICES + 2).map(|i| notice(&format!("n{i}"))));
        assert_eq!(
            overflowed.session_notices.len(),
            AcpState::MAX_SESSION_NOTICES
        );
        assert_eq!(titles(&overflowed), ["n2", "n3", "n4"], "oldest drop first");

        assert!(
            titles(&applied([notice("stale"), prompt("next")])).is_empty(),
            "a new turn retires the previous turn's advisories"
        );
        assert!(
            titles(&applied([
                notice("stale"),
                Event::AgentSwitched {
                    from: "claude".into(),
                    to: "codex".into(),
                    reason: "user".into(),
                },
            ]))
            .is_empty(),
            "the prior agent's advisories do not carry over"
        );
    }

    fn auth(kind: AuthStatusKind, label: &str) -> AuthStatus {
        AuthStatus {
            kind,
            label: label.into(),
            detail: None,
            account: None,
        }
    }

    #[test]
    fn auth_status_tracks_the_latest_report_and_clears() {
        use self::applied_current_session as applied;
        let max = auth(AuthStatusKind::Account, "Claude Max");
        let key = auth(AuthStatusKind::ApiKey, "Anthropic API key");

        // Never reported is not the same as logged out: it renders as nothing.
        assert_eq!(fresh_state().auth_status, None);

        let s = applied([Event::AuthStatusUpdated {
            status: Some(max.clone()),
        }]);
        assert_eq!(s.auth_status.as_ref(), Some(&max));

        // A later report replaces the earlier one wholesale.
        let s = applied([
            Event::AuthStatusUpdated {
                status: Some(max.clone()),
            },
            Event::AuthStatusUpdated {
                status: Some(key.clone()),
            },
        ]);
        assert_eq!(s.auth_status.as_ref(), Some(&key));

        // An adapter that cannot report clears the previous process's value
        // rather than leaving it on screen.
        let s = applied([
            Event::AuthStatusUpdated { status: Some(max) },
            Event::AuthStatusUpdated { status: None },
        ]);
        assert_eq!(s.auth_status, None);
    }

    #[test]
    fn auth_status_drops_the_vendor_bag_but_keeps_the_account() {
        let status: AuthStatus = serde_json::from_value(serde_json::json!({
            "kind": "account",
            "label": "Claude Max",
            "account": {"email": "a@b.co", "organization": "Acme", "plan": "max"},
            "vendor": {"claudeCode": {"secret": "x"}},
        }))
        .unwrap();
        assert_eq!(
            status.account.as_ref().unwrap().email.as_deref(),
            Some("a@b.co")
        );
        let round_tripped = serde_json::to_value(&status).unwrap();
        assert!(
            round_tripped.get("vendor").is_none(),
            "vendor must not reach the event log: {round_tripped}"
        );
    }

    fn caps(steering: bool) -> Event {
        Event::PromptCapabilities {
            image: false,
            audio: false,
            embedded_context: false,
            load_session: None,
            steering,
        }
    }

    fn cancel() -> Event {
        Event::CancelRequested {
            escalates_at: Utc::now(),
        }
    }

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "Read".into(),
            kind: "read".into(),
            args_preview: "{}".into(),
            started_at: Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        }
    }

    fn diff(path: &str) -> DiffPreview {
        DiffPreview {
            path: path.into(),
            old_text: None,
            new_text: Some("new".into()),
            created_at: Utc::now(),
        }
    }

    fn tool_update(title: Option<&str>, diffs: Option<Vec<DiffPreview>>) -> Event {
        Event::ToolCallUpdated {
            tool_call_id: "tc-1".into(),
            title: title.map(Into::into),
            args_preview: None,
            started_at: None,
            diffs,
        }
    }

    fn switch_agent() -> Event {
        Event::AgentSwitched {
            from: "claude".into(),
            to: "codex".into(),
            reason: "rate_limit".into(),
        }
    }

    fn rate_limit() -> Event {
        Event::RateLimit {
            info: RateLimitInfo::undated(),
        }
    }

    #[test]
    fn legacy_and_open_serde_shapes_still_decode() {
        let event: Event = serde_json::from_value(serde_json::json!({
            "PromptCapabilities": {
                "image": false,
                "audio": false,
                "embedded_context": false,
                "steering": false
            }
        }))
        .unwrap();
        assert!(matches!(
            event,
            Event::PromptCapabilities {
                load_session: None,
                ..
            }
        ));

        for (json, want) in [
            ("\"model\"", ConfigOptionCategory::Model),
            ("\"thought_level\"", ConfigOptionCategory::ThoughtLevel),
            (
                "\"future_category\"",
                ConfigOptionCategory::Other("future_category".into()),
            ),
        ] {
            assert_eq!(
                serde_json::from_str::<ConfigOptionCategory>(json).unwrap(),
                want
            );
        }
        let back = serde_json::to_string(&ConfigOptionCategory::Other("x".into())).unwrap();
        assert_eq!(back, "\"x\"");
    }

    #[test]
    fn turn_flags_and_rate_limit_park_follow_their_edges() {
        use self::applied_current_session as applied;
        let mut s = fresh_state();
        assert!(!s.turn_active);
        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &prompt("hi"),
            &current_session_metadata(),
        )
        .unwrap();
        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &Event::ThinkingStarted,
            &current_session_metadata(),
        )
        .unwrap();
        assert!(s.turn_active && s.thinking.is_some());
        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &stopped("end_turn"),
            &current_session_metadata(),
        )
        .unwrap();
        assert!(!s.turn_active && s.thinking.is_none());

        for terminal in [
            Event::AgentStartupError {
                message: "boom".into(),
                native_subject: None,
            },
            Event::PromptRuntimeError {
                message: "boom".into(),
                evidence: None,
                native_subject: None,
            },
            Event::PromptRejected {
                reason: "agent_busy".into(),
                text: "hi".into(),
            },
        ] {
            let s = applied([prompt("hi"), terminal.clone()]);
            assert!(!s.turn_active, "{terminal:?} clears turn_active");
        }

        assert!(applied([caps(true)]).steering);
        assert!(!applied([caps(true), caps(false)]).steering);

        assert!(applied([prompt("hi"), cancel()]).cancelling);
        assert!(!applied([prompt("hi"), cancel(), stopped("cancelled")]).cancelling);
        assert!(!applied([prompt("one"), cancel(), prompt("fresh turn")]).cancelling);
        assert!(
            applied([prompt("one"), cancel(), caps(true), prompt("steered")]).cancelling,
            "a steered continuation keeps cancelling"
        );

        let s = applied([Event::ConversationCompactionStarted]);
        assert!(s.compacting);
        assert!(
            !applied([
                Event::ConversationCompactionStarted,
                Event::ConversationCompacted
            ])
            .compacting
        );
        assert!(
            !applied([Event::ConversationCompactionStarted, stopped("end_turn")]).compacting,
            "Stopped self-heals a stuck compaction"
        );

        let compact_tool = || {
            let mut tc = tool_call("tc-compact");
            tc.name = COMPACT_TOOL_NAME.into();
            Event::ToolCallStarted { tool_call: tc }
        };
        let tool_done = |id: &str, is_error: bool| Event::ToolCallCompleted {
            tool_call_id: id.into(),
            is_error,
            content: String::new(),
            output: Vec::new(),
            completed_at: Utc::now(),
            async_subagent: false,
        };
        assert!(
            applied([compact_tool()]).compacting,
            "the adapter's compact tool call marks compaction"
        );
        assert!(
            !applied([compact_tool(), tool_done("tc-compact", true)]).compacting,
            "a failed compaction clears the flag"
        );
        assert!(
            applied([compact_tool(), tool_done("other", false)]).compacting,
            "another tool's completion leaves it set"
        );
        let retitle = Event::ToolCallUpdated {
            tool_call_id: "tc-compact".into(),
            title: Some("Compacting context".into()),
            args_preview: None,
            started_at: None,
            diffs: None,
        };
        assert!(
            !applied([compact_tool(), retitle, tool_done("tc-compact", false)]).compacting,
            "a retitled compact call still clears by id"
        );
        assert!(
            !applied([compact_tool(), Event::ConversationCompactionFailed]).compacting,
            "a reported failure clears the flag"
        );
        assert!(
            !applied([
                Event::ConversationCompactionStarted,
                Event::ConversationCompactionFailed
            ])
            .compacting,
            "a failure also ends a text-marked compaction"
        );

        let mut s = fresh_state();
        let before = s.updated_at;
        let seq = s
            .apply_scoped_event(
                s.last_seq.saturating_add(1),
                &Event::ModeSwitchFailed {
                    mode_id: "bypassPermissions".into(),
                    reason: "Mode bypassPermissions is not available.".into(),
                },
                &current_session_metadata(),
            )
            .unwrap();
        assert_eq!(seq, 1);
        assert_eq!(
            s.mode,
            SessionMode::Default,
            "a failed switch changes nothing"
        );
        assert!(s.updated_at >= before);
        let result = s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &Event::ApprovalResolved {
                nonce: Nonce::new(),
                decision: ApprovalDecision::Allow,
            },
            &current_session_metadata(),
        );
        assert!(matches!(result, Err(StateError::UnknownApprovalNonce(_))));

        // A rate-limit park ends on a live prompt, session, or organic stop.
        assert!(applied([rate_limit()]).rate_limit.is_some());
        let resumed = Event::RateLimitAutoResumed {
            resets_at: Utc::now(),
            manual: false,
        };
        assert!(
            applied([rate_limit(), resumed]).rate_limit.is_some(),
            "a resume that has not come up yet keeps the park"
        );
        assert!(applied([rate_limit(), stopped("rate_limited")])
            .rate_limit
            .is_some());
        assert!(
            applied([
                rate_limit(),
                Event::AcpSessionAssigned {
                    acp_session_id: "binding-only".into(),
                    connection_ready: false,
                    native_subject: None,
                }
            ])
            .rate_limit
            .is_some(),
            "an identity hint is not a live-channel declaration"
        );
        assert!(
            applied([
                rate_limit(),
                Event::AcpSessionAssigned {
                    acp_session_id: "unqualified".into(),
                    connection_ready: true,
                    native_subject: None,
                }
            ])
            .rate_limit
            .is_some(),
            "a bare ready flag does not end a native park"
        );
        let assigned = observed_channel_ready("acp-2");
        for end in [
            prompt("go"),
            assigned,
            stopped("user_stopped"),
            switch_agent(),
        ] {
            assert!(
                applied([rate_limit(), end.clone()]).rate_limit.is_none(),
                "{end:?}"
            );
        }
    }

    #[test]
    fn tool_calls_keep_their_diffs_across_updates_and_repeated_starts() {
        use self::applied_current_session as applied;
        let s = applied(
            (0..AcpState::MAX_RECENT_DIFFS + 5).map(|i| Event::DiffEmitted {
                diff: diff(&format!("/tmp/file{i}.txt")),
            }),
        );
        assert_eq!(s.recent_diffs.len(), AcpState::MAX_RECENT_DIFFS);
        assert!(
            s.recent_diffs[0].path.contains("file5"),
            "oldest dropped first"
        );

        let completed = Event::ToolCallCompleted {
            tool_call_id: "tc-1".into(),
            is_error: false,
            content: String::new(),
            output: Vec::new(),
            completed_at: Utc::now(),
            async_subagent: false,
        };
        assert!(applied([Event::ToolCallStarted {
            tool_call: tool_call("tc-1")
        }])
        .in_flight_tool
        .is_some());
        assert!(applied([
            Event::ToolCallStarted {
                tool_call: tool_call("tc-1")
            },
            completed
        ])
        .in_flight_tool
        .is_none());

        let mut s = applied([
            Event::ToolCallStarted {
                tool_call: tool_call("tc-1"),
            },
            tool_update(None, Some(vec![diff("src/foo.rs")])),
            tool_update(Some("Edit src/foo.rs"), None),
        ]);
        let tool = s.in_flight_tool.as_ref().unwrap();
        assert_eq!(tool.name, "Edit src/foo.rs");
        assert_eq!(tool.diffs.len(), 1, "a text-only update keeps the diffs");

        let mut richer = tool_call("tc-1");
        richer.name = "Write src/foo.rs".into();
        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &Event::ToolCallStarted { tool_call: richer },
            &current_session_metadata(),
        )
        .unwrap();
        let tool = s.in_flight_tool.as_ref().unwrap();
        assert_eq!(tool.name, "Write src/foo.rs", "richer fields still apply");
        assert_eq!(
            tool.diffs[0].path, "src/foo.rs",
            "the repeated start keeps the diff"
        );

        // Phases clear when a tool or question takes over.
        let elicitation = |nonce: &str, tool_call_id: &str| Event::ElicitationRequested {
            elicitation: Elicitation {
                nonce: Nonce(nonce.into()),
                message: "Which one?".into(),
                title: None,
                description: None,
                tool_call_id: Some(tool_call_id.into()),
                questions: Vec::new(),
                requested_at: Utc::now(),
                resolved: None,
            },
        };
        let start = |id: &str| Event::ToolCallStarted {
            tool_call: tool_call(id),
        };
        assert!(
            applied([Event::ThinkingStarted, start("t-1")])
                .thinking
                .is_none(),
            "a tool call ends the reasoning block"
        );
        assert!(applied([start("ask-1"), elicitation("n-1", "ask-1")])
            .in_flight_tool
            .is_none());
        let s = applied([start("t-2"), elicitation("n-2", "other")]);
        assert_eq!(s.in_flight_tool.map(|t| t.id).as_deref(), Some("t-2"));
    }

    #[test]
    fn background_agent_lifecycle_builds_record() {
        let progress = |tool_count, tools| Event::BackgroundAgentProgress {
            agent_id: "a1".into(),
            status: BackgroundAgentStatus::Running,
            tool_count,
            tools,
            last_tool: Some("Read".into()),
            last_text: None,
            at: Utc::now(),
        };
        let mut s = applied([
            Event::BackgroundAgentLaunched {
                agent_id: "a1".into(),
                tool_call_id: "tc1".into(),
                description: "map backend".into(),
                prompt: "do the thing".into(),
                model: "claude-opus-4-8".into(),
                output_file: "/tmp/a1.output".into(),
                started_at: Utc::now(),
                workflow: None,
            },
            progress(
                3,
                vec![BackgroundAgentTool {
                    name: "Read".into(),
                    title: Some("x.rs".into()),
                    ok: Some(true),
                }],
            ),
        ]);
        let agent = &s.background_agents[0];
        assert_eq!(
            (agent.status, agent.tool_count),
            (BackgroundAgentStatus::Running, 3)
        );
        assert_eq!(agent.tool_call_id, "tc1");
        assert_eq!(agent.tools[0].name, "Read");
        assert_eq!(agent.last_tool.as_deref(), Some("Read"));

        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &Event::BackgroundAgentCompleted {
                agent_id: "a1".into(),
                status: BackgroundAgentStatus::Completed,
                tools: vec![],
                result: Some("done".into()),
                warning: None,
                ended_at: Utc::now(),
            },
            &crate::acp::event_sender::EventMetadata::history(),
        )
        .unwrap();
        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &progress(9, vec![]),
            &crate::acp::event_sender::EventMetadata::history(),
        )
        .unwrap();
        let agent = &s.background_agents[0];
        assert_eq!(
            agent.status,
            BackgroundAgentStatus::Completed,
            "late progress cannot reopen"
        );
        assert_eq!((agent.tool_count, agent.tools.len()), (3, 1));
        assert_eq!(agent.result.as_deref(), Some("done"));

        // A stalled Progress sets no `ended_at`, so the next one resumes it.
        let s = applied([launched(), bg_progress(BackgroundAgentStatus::Stalled, 4)]);
        assert_eq!(
            s.background_agents[0].status,
            BackgroundAgentStatus::Stalled
        );
        assert!(s.background_agents[0].ended_at.is_none());
        assert!(s.has_active_background_agent());

        let s = applied([
            launched(),
            bg_progress(BackgroundAgentStatus::Stalled, 4),
            bg_progress(BackgroundAgentStatus::Running, 5),
        ]);
        assert_eq!(
            s.background_agents[0].status,
            BackgroundAgentStatus::Running
        );
    }

    fn launched() -> Event {
        Event::BackgroundAgentLaunched {
            agent_id: "a1".into(),
            tool_call_id: "tc1".into(),
            description: "map backend".into(),
            prompt: "do the thing".into(),
            model: "claude-opus-4-8".into(),
            output_file: "/tmp/a1.output".into(),
            started_at: Utc::now(),
            workflow: None,
        }
    }

    fn bg_progress(status: BackgroundAgentStatus, tool_count: u32) -> Event {
        Event::BackgroundAgentProgress {
            agent_id: "a1".into(),
            status,
            tool_count,
            tools: vec![],
            last_tool: None,
            last_text: None,
            at: Utc::now(),
        }
    }

    fn bg_completed(status: BackgroundAgentStatus) -> Event {
        Event::BackgroundAgentCompleted {
            agent_id: "a1".into(),
            status,
            tools: vec![],
            result: None,
            warning: None,
            ended_at: Utc::now(),
        }
    }

    /// #4001: a completion closes the record whatever status it carries, and
    /// a late Progress never reopens it. `Stalled` is the case the old
    /// status-only guard missed: the tailer's abort timeout ends a run with a
    /// status that is not otherwise terminal, which both wedged the busy
    /// signal on and let a straggling Progress revive the record.
    #[test]
    fn a_terminal_background_record_never_reopens_and_goes_idle() {
        for status in [
            BackgroundAgentStatus::Stalled,
            BackgroundAgentStatus::Detached,
            BackgroundAgentStatus::Completed,
        ] {
            let mut s = applied([launched()]);
            assert!(s.has_active_background_agent(), "{status:?}");

            s.apply_scoped_event(
                s.last_seq.saturating_add(1),
                &bg_completed(status),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
            s.apply_scoped_event(
                s.last_seq.saturating_add(1),
                &bg_progress(BackgroundAgentStatus::Running, 9),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
            let agent = &s.background_agents[0];
            assert_eq!(agent.status, status, "{status:?} must not reopen");
            assert!(agent.ended_at.is_some(), "{status:?}");
            assert!(!s.has_active_background_agent(), "{status:?}");
        }
    }

    /// A backgrounded command keeps the session busy after the turn that
    /// started it ends, until its own finish event.
    #[test]
    fn a_background_task_holds_the_busy_signal_until_it_finishes() {
        let started = |id: &str| Event::BackgroundTaskStarted {
            task_id: id.into(),
            tool_call_id: None,
            description: "sleep".into(),
            workflow: None,
        };
        let finished = |id: &str| Event::BackgroundTaskFinished {
            task_id: id.into(),
            ended_at: None,
        };
        let mut s = applied([prompt("go"), started("t-1"), started("t-2"), stopped("end")]);
        assert!(!s.turn_active);
        assert!(s.has_active_background_agent());

        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &finished("t-1"),
            &crate::acp::event_sender::EventMetadata::history(),
        )
        .unwrap();
        assert!(s.has_active_background_agent(), "t-2 still runs");
        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &finished("t-2"),
            &crate::acp::event_sender::EventMetadata::history(),
        )
        .unwrap();
        assert!(!s.has_active_background_agent());
    }

    #[test]
    fn workflow_runs_keep_their_end_after_their_task() {
        let workflow = |id: &str, call: &str| Event::BackgroundTaskStarted {
            task_id: id.into(),
            tool_call_id: Some(call.into()),
            description: "drafts".into(),
            workflow: Some(WorkflowLaunch {
                run_id: "wf_1".into(),
                name: "drafts".into(),
                summary: None,
                transcript_dir: "/home/u/.claude/wf".into(),
            }),
        };
        let runs = |s: &AcpState| {
            s.workflow_runs
                .iter()
                .map(|run| {
                    (
                        run.tool_call_id.clone(),
                        run.finished,
                        run.ended_at.map(|at| at.to_rfc3339()),
                    )
                })
                .collect::<Vec<_>>()
        };
        let ended_at: Option<DateTime<Utc>> = "2026-10-30T10:01:00Z".parse().ok();
        let mut s = applied([workflow("wfx", "toolu_a"), workflow("wfy", "toolu_b")]);
        assert_eq!(
            runs(&s),
            [
                ("toolu_a".into(), false, None),
                ("toolu_b".into(), false, None)
            ]
        );
        for event in [
            Event::BackgroundTaskStarted {
                task_id: "wfx".into(),
                tool_call_id: Some("toolu_bash".into()),
                description: "sleep".into(),
                workflow: None,
            },
            Event::BackgroundTaskFinished {
                task_id: "wfy".into(),
                ended_at,
            },
            // The command reusing `wfx` ends; the replaced run keeps no time.
            Event::BackgroundTaskFinished {
                task_id: "wfx".into(),
                ended_at,
            },
        ] {
            s.apply_scoped_event(
                s.last_seq.saturating_add(1),
                &event,
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        }
        assert_eq!(
            runs(&s),
            [
                ("toolu_a".into(), true, None),
                (
                    "toolu_b".into(),
                    true,
                    Some("2026-10-30T10:01:00+00:00".into())
                ),
            ]
        );
    }

    /// #4001: `turn_active` gates prompt dispatch and the queue drain, not
    /// just display, so `Stopped` clears it even while a sub-agent the turn
    /// spawned runs on. The busy display signal is the pair of flags.
    #[test]
    fn turn_active_tracks_only_the_main_turn() {
        use self::applied_current_session as applied;
        let mut s = applied([prompt("go"), launched()]);
        assert!(s.turn_active && s.has_active_background_agent());

        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &bg_completed(BackgroundAgentStatus::Completed),
            &crate::acp::event_sender::EventMetadata::history(),
        )
        .unwrap();
        assert!(
            s.turn_active,
            "the main turn's own Stopped never fired, so it is still live"
        );
        assert!(!s.has_active_background_agent());

        let mut s = applied([prompt("go"), launched(), stopped("prompt_complete")]);
        assert!(
            !s.turn_active,
            "dispatch must send the next prompt rather than queue it behind a background agent"
        );
        assert!(
            s.has_active_background_agent(),
            "the display signal stays busy"
        );

        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &bg_completed(BackgroundAgentStatus::Completed),
            &crate::acp::event_sender::EventMetadata::history(),
        )
        .unwrap();
        assert!(!s.turn_active && !s.has_active_background_agent());
    }

    fn config_options(model: &str) -> Event {
        let choice = |value: &str| ConfigOptionChoice {
            value: value.into(),
            name: value.to_uppercase(),
            description: None,
        };
        Event::ConfigOptionsUpdated {
            options: vec![
                ConfigOptionDescriptor {
                    id: "model".into(),
                    name: "Model".into(),
                    description: None,
                    category: ConfigOptionCategory::Model,
                    current_value: model.into(),
                    options: vec![choice("claude-opus-4-7"), choice("claude-sonnet-4-6")],
                },
                ConfigOptionDescriptor {
                    id: "effort".into(),
                    name: "Reasoning Effort".into(),
                    description: None,
                    category: ConfigOptionCategory::ThoughtLevel,
                    current_value: "default".into(),
                    options: vec![choice("default"), choice("high")],
                },
            ],
        }
    }

    #[test]
    fn config_option_failure_notice_clears_only_when_the_value_lands() {
        use self::applied_current_session as applied;
        let failed = || Event::ConfigOptionSwitchFailed {
            config_id: "model".into(),
            value: "claude-sonnet-4-6".into(),
            reason: "rate limited".into(),
        };
        let s = applied([config_options("claude-opus-4-7"), failed()]);
        assert_eq!(
            s.config_option_switch_failed,
            Some(ConfigOptionSwitchFailure {
                config_id: "model".into(),
                value: "claude-sonnet-4-6".into(),
                reason: "rate limited".into(),
            })
        );
        assert_eq!(s.config_options[0].current_value, "claude-opus-4-7");

        let s = applied([
            config_options("claude-opus-4-7"),
            failed(),
            config_options("claude-opus-4-7"),
        ]);
        assert!(
            s.config_option_switch_failed.is_some(),
            "an unrelated snapshot keeps it"
        );
        let s = applied([
            config_options("claude-opus-4-7"),
            failed(),
            config_options("claude-sonnet-4-6"),
        ]);
        assert!(s.config_option_switch_failed.is_none());
        assert_eq!(s.config_options[0].current_value, "claude-sonnet-4-6");
    }

    #[test]
    fn clear_keeps_adapter_capabilities_and_a_switch_drops_them() {
        use self::applied_current_session as applied;
        let adapter_events = || {
            [
                config_options("claude-opus-4-7"),
                Event::AvailableCommandsUpdated {
                    commands: vec![AvailableCommand {
                        name: "review".into(),
                        description: "Review".into(),
                        accepts_input: true,
                    }],
                },
                Event::ModesAvailable {
                    current_mode_id: "default".into(),
                    modes: vec![ModeInfo {
                        id: "plan".into(),
                        name: "Plan".into(),
                        description: None,
                    }],
                },
                Event::CurrentModeChanged {
                    current_mode_id: "plan".into(),
                },
                Event::UsageUpdated {
                    usage: SessionUsage {
                        used: 5_000,
                        size: 200_000,
                        cost: Some(UsageCost {
                            amount: 0.12,
                            currency: "USD".into(),
                        }),
                    },
                },
            ]
        };
        let s = applied(adapter_events());
        assert_eq!(
            s.usage
                .as_ref()
                .and_then(|u| u.cost.as_ref())
                .map(|c| c.currency.as_str()),
            Some("USD")
        );

        let mut s = applied(adapter_events());
        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &Event::SessionCleared,
            &current_session_metadata(),
        )
        .unwrap();
        assert_eq!(s.config_options.len(), 2);
        assert!(s.available_commands[0].accepts_input);
        assert_eq!(s.available_modes.len(), 1);
        assert_eq!(s.current_mode_id.as_deref(), Some("plan"));
        assert!(s.usage.is_none() && s.current_plan.is_none());

        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &Event::ConfigOptionSwitchFailed {
                config_id: "effort".into(),
                value: "high".into(),
                reason: "unsupported".into(),
            },
            &current_session_metadata(),
        )
        .unwrap();
        s.apply_scoped_event(
            s.last_seq.saturating_add(1),
            &switch_agent(),
            &current_session_metadata(),
        )
        .unwrap();
        assert!(s.config_options.is_empty() && s.config_option_switch_failed.is_none());
        assert!(s.available_commands.is_empty() && s.available_modes.is_empty());
        assert_eq!(s.current_mode_id, None);
        assert_eq!(s.agent.0, "codex");
    }

    #[test]
    fn certified_clear_retains_only_new_plan_across_cold_state() {
        let plan = |title: &str| Event::PlanUpdated {
            plan: Plan {
                plan_id: title.into(),
                version: 1,
                steps: vec![PlanStep {
                    id: title.into(),
                    title: title.into(),
                    detail: None,
                    status: PlanStepStatus::Pending,
                }],
            },
        };
        for (case, cut, new_seq, retained) in [
            ("old plan", Some(5), None, false),
            ("new prefix plan", Some(5), Some(7), true),
            ("plan on old boundary", Some(7), Some(7), false),
            ("legacy clear", None, Some(7), false),
        ] {
            let mut state = fresh_state();
            state
                .apply_current_event(
                    1,
                    &plan("old"),
                    crate::acp::operation_effect::ControlKind::Other,
                    None,
                    None,
                )
                .unwrap();
            if let Some(seq) = new_seq {
                state
                    .apply_current_event(
                        seq,
                        &plan("new"),
                        crate::acp::operation_effect::ControlKind::Other,
                        None,
                        None,
                    )
                    .unwrap();
            }
            let mut state: AcpState =
                serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
            state
                .apply_current_event(
                    9,
                    &Event::SessionCleared,
                    crate::acp::operation_effect::ControlKind::Cleared,
                    None,
                    cut,
                )
                .unwrap();
            assert_eq!(
                state.sidebar_observation_seq, 0,
                "a display clear is not a terminal observation"
            );
            assert_eq!(
                state
                    .current_plan
                    .as_ref()
                    .map(|plan| plan.steps[0].title.as_str()),
                retained.then_some("new"),
                "{case}"
            );
            state
                .apply_current_event(
                    10,
                    &Event::SessionCleared,
                    crate::acp::operation_effect::ControlKind::Cleared,
                    None,
                    Some(9),
                )
                .unwrap();
            assert!(
                state.current_plan.is_none(),
                "{case}: later clear removes prior plan"
            );
        }
    }
}

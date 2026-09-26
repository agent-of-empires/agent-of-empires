//! `AcpState`: the structured view session state, folded from `Event`s by a single writer.

use crate::daemon::PromptAttachmentRef;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// The parent `Task` tool call that launched this agent.
    pub tool_call_id: String,
    pub description: String,
    pub prompt: String,
    pub model: String,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AcpState {
    pub session_id: AcpSessionId,
    pub agent: AgentName,
    pub model: Option<String>,
    pub mode: SessionMode,

    pub current_plan: Option<Plan>,
    pub todos: Vec<Todo>,
    pub in_flight_tool: Option<ToolCall>,
    pub pending_approvals: Vec<Approval>,
    #[serde(default)]
    pub pending_elicitations: Vec<Elicitation>,
    #[serde(default)]
    pub recent_diffs: Vec<DiffPreview>,
    pub thinking: Option<ThinkingSignal>,
    pub rate_limit: Option<RateLimitInfo>,
    #[serde(default)]
    pub usage: Option<SessionUsage>,
    #[serde(default)]
    pub available_commands: Vec<AvailableCommand>,
    /// Modes the adapter advertised; adapter-scoped, so they survive `/clear`.
    #[serde(default)]
    pub available_modes: Vec<ModeInfo>,
    #[serde(default)]
    pub current_mode_id: Option<String>,
    #[serde(default)]
    pub last_agent_switch: Option<AgentSwitchInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_error: Option<StartupErrorDetail>,
    /// Full snapshot of the adapter's per-session selectors.
    #[serde(default)]
    pub config_options: Vec<ConfigOptionDescriptor>,
    /// Notice for the most recent rejected `session/set_config_option`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_option_switch_failed: Option<ConfigOptionSwitchFailure>,
    #[serde(default)]
    pub background_agents: Vec<BackgroundAgentRecord>,

    /// Whether the main turn is in flight. Dispatch (`acp::dispatch::decide`)
    /// and the queue drain gate on this directly, so it tracks only the main
    /// turn, never a background sub-agent; display signals combine it with
    /// `has_active_background_agent()`.
    #[serde(default)]
    pub turn_active: bool,
    /// A mid-turn prompt is injected into the running turn rather than queued.
    #[serde(default)]
    pub steering: bool,
    /// A cancel was requested and its terminal `Stopped` has not arrived.
    #[serde(default)]
    pub cancelling: bool,
    /// A `/compact` is running; the adapter goes silent for minutes.
    #[serde(default)]
    pub compacting: bool,

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
    /// The adapter runtime failed a prompt before emitting any transcript.
    PromptRuntimeError {
        message: String,
    },
    AgentMessageChunk {
        text: String,
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
    },
    /// Native context continuity was lost or a fork could not be established.
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

    pub fn new(session_id: AcpSessionId, agent: AgentName, model: Option<String>) -> Self {
        Self {
            session_id,
            agent,
            model,
            updated_at: Utc::now(),
            ..Self::default()
        }
    }

    /// Whether any background sub-agent is still in flight. Keyed on
    /// `ended_at`, not `status`: a terminal `BackgroundAgentCompleted` can
    /// carry `status: Stalled` (the tailer's abort timeout gives up without a
    /// clean `end_turn`), and that record is done like any other (#4001).
    pub fn has_active_background_agent(&self) -> bool {
        self.background_agents.iter().any(|a| a.ended_at.is_none())
    }

    /// Apply a single event; returns the new `last_seq`.
    pub fn apply_event(&mut self, event: Event) -> Result<u64, StateError> {
        match event {
            Event::PlanUpdated { plan } => self.current_plan = Some(plan),
            Event::TodoListUpdated { todos } => self.todos = todos,
            Event::ToolCallStarted { tool_call } => self.start_tool_call(tool_call),
            Event::ToolCallCompleted { tool_call_id, .. } => {
                if self
                    .in_flight_tool
                    .as_ref()
                    .is_some_and(|t| t.id == tool_call_id)
                {
                    self.in_flight_tool = None;
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
                    .filter(|t| t.id == tool_call_id)
                {
                    if let Some(title) = title {
                        tool.name = title;
                    }
                    if let Some(args_preview) = args_preview {
                        tool.args_preview = args_preview;
                    }
                    if let Some(started_at) = started_at {
                        tool.started_at = started_at;
                    }
                    if let Some(diffs) = diffs {
                        tool.diffs = diffs;
                    }
                }
            }
            Event::ApprovalRequested { approval } => self.pending_approvals.push(approval),
            Event::ApprovalResolved { nonce, .. } => {
                let pos = self
                    .pending_approvals
                    .iter()
                    .position(|a| a.nonce == nonce)
                    .ok_or_else(|| StateError::UnknownApprovalNonce(nonce.clone()))?;
                if self.pending_approvals.remove(pos).resolved.is_some() {
                    return Err(StateError::ApprovalAlreadyResolved(nonce));
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
                self.pending_elicitations.push(elicitation)
            }
            Event::ElicitationResolved { nonce, .. } => {
                self.pending_elicitations.retain(|e| e.nonce != nonce);
            }
            Event::DiffEmitted { diff } => {
                self.recent_diffs.push(diff);
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
                self.turn_active = true;
            }
            Event::ThinkingEnded => self.thinking = None,
            Event::RateLimit { info } => self.rate_limit = Some(info),
            Event::UsageUpdated { usage } => self.usage = Some(usage),
            Event::ModeChanged { mode } => self.mode = mode,
            Event::ModesAvailable {
                current_mode_id,
                modes,
            } => {
                self.available_modes = modes;
                self.current_mode_id = Some(current_mode_id);
            }
            Event::CurrentModeChanged { current_mode_id } => {
                self.current_mode_id = Some(current_mode_id)
            }
            Event::AvailableCommandsUpdated { commands } => self.available_commands = commands,
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
                self.config_options = options;
            }
            Event::ConfigOptionSwitchFailed {
                config_id,
                value,
                reason,
            } => {
                self.config_option_switch_failed = Some(ConfigOptionSwitchFailure {
                    config_id,
                    value,
                    reason,
                });
            }
            Event::PromptRuntimeError { .. } | Event::AgentStartupError { .. } => {
                self.turn_active = false;
                self.cancelling = false;
            }
            Event::CancelRequested { .. } => self.cancelling = true,
            Event::Stopped { reason } => self.end_turn(&reason),
            Event::IncompatibleAgent { detail } => self.startup_error = Some(detail),
            Event::UserPromptSent { .. } | Event::UserDiffCommentsPrompt { .. } => self.open_turn(),
            Event::PromptCapabilities { steering, .. } => self.steering = steering,
            Event::AcpSessionAssigned { .. } => {
                self.startup_error = None;
                self.rate_limit = None;
            }
            Event::SessionContextReset { .. } => self.usage = None,
            // Clears the conversation; adapter-scoped commands, modes and
            // selectors are never re-announced, so they stay.
            Event::SessionCleared => {
                self.usage = None;
                self.current_plan = None;
                self.mode = SessionMode::Default;
                self.pending_approvals = Vec::new();
                self.pending_elicitations = Vec::new();
            }
            Event::ConversationCompactionStarted => self.compacting = true,
            Event::ConversationCompacted => {
                self.compacting = false;
                self.usage = None;
            }
            Event::PromptRejected { .. } => self.turn_active = false,
            Event::AgentSwitched { from, to, reason } => self.switch_agent(from, to, reason),
            Event::BackgroundAgentLaunched {
                agent_id,
                tool_call_id,
                description,
                prompt,
                model,
                output_file: _,
                started_at,
            } => self.launch_background_agent(BackgroundAgentRecord {
                agent_id,
                tool_call_id,
                description,
                prompt,
                model,
                status: BackgroundAgentStatus::Running,
                started_at,
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
                // A terminal record never reopens. `ended_at` is part of the
                // guard because the tailer's abort timeout closes a record
                // with the non-terminal `Stalled` status.
                if let Some(a) = self
                    .background_agent(&agent_id)
                    .filter(|a| a.ended_at.is_none() && !a.status.is_terminal())
                {
                    a.status = status;
                    a.tool_count = tool_count;
                    replace_if_some(&mut a.last_tool, last_tool);
                    replace_if_some(&mut a.last_text, last_text);
                    if !tools.is_empty() {
                        a.tools = tools;
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
                if let Some(a) = self.background_agent(&agent_id) {
                    a.status = status;
                    a.ended_at = Some(ended_at);
                    replace_if_some(&mut a.result, result);
                    replace_if_some(&mut a.warning, warning);
                    if !tools.is_empty() {
                        a.tools = tools;
                    }
                }
            }
            // Titles live on `Instance`; wakeups and monitors are read from the log.
            Event::SessionTitleSuggested { .. }
            | Event::ToolCallContent { .. }
            | Event::RateLimitAutoResumed { .. }
            | Event::ModeSwitchFailed { .. }
            | Event::RawAgentUpdate { .. }
            | Event::AgentMessageChunk { .. }
            | Event::ConversationSummary { .. }
            | Event::WakeupScheduled { .. }
            | Event::MonitorArmed { .. } => {}
        }
        self.last_seq = self.last_seq.saturating_add(1);
        self.updated_at = Utc::now();
        Ok(self.last_seq)
    }

    fn start_tool_call(&mut self, tool_call: ToolCall) {
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
        if !steered {
            self.cancelling = false;
        }
        self.rate_limit = None;
    }

    /// The turn is over however it ended, so every in-turn phase clears.
    fn end_turn(&mut self, reason: &str) {
        self.turn_active = false;
        self.cancelling = false;
        self.compacting = false;
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
        self.in_flight_tool = None;
        self.thinking = None;
        self.pending_approvals = Vec::new();
        self.pending_elicitations = Vec::new();
        self.usage = None;
        self.available_commands = Vec::new();
        self.available_modes = Vec::new();
        self.current_mode_id = None;
        self.current_plan = None;
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
        }
    }

    pub(crate) fn chunk(text: &str) -> Event {
        Event::AgentMessageChunk { text: text.into() }
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
            s.apply_event(event).unwrap();
        }
        s
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
        let mut s = fresh_state();
        assert!(!s.turn_active);
        s.apply_event(prompt("hi")).unwrap();
        s.apply_event(Event::ThinkingStarted).unwrap();
        assert!(s.turn_active && s.thinking.is_some());
        s.apply_event(stopped("end_turn")).unwrap();
        assert!(!s.turn_active && s.thinking.is_none());

        for terminal in [
            Event::AgentStartupError {
                message: "boom".into(),
            },
            Event::PromptRuntimeError {
                message: "boom".into(),
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

        let mut s = fresh_state();
        let before = s.updated_at;
        let seq = s
            .apply_event(Event::ModeSwitchFailed {
                mode_id: "bypassPermissions".into(),
                reason: "Mode bypassPermissions is not available.".into(),
            })
            .unwrap();
        assert_eq!(seq, 1);
        assert_eq!(
            s.mode,
            SessionMode::Default,
            "a failed switch changes nothing"
        );
        assert!(s.updated_at >= before);
        let result = s.apply_event(Event::ApprovalResolved {
            nonce: Nonce::new(),
            decision: ApprovalDecision::Allow,
        });
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
        let assigned = Event::AcpSessionAssigned {
            acp_session_id: "acp-2".into(),
        };
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
        s.apply_event(Event::ToolCallStarted { tool_call: richer })
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

        s.apply_event(Event::BackgroundAgentCompleted {
            agent_id: "a1".into(),
            status: BackgroundAgentStatus::Completed,
            tools: vec![],
            result: Some("done".into()),
            warning: None,
            ended_at: Utc::now(),
        })
        .unwrap();
        s.apply_event(progress(9, vec![])).unwrap();
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

            s.apply_event(bg_completed(status)).unwrap();
            s.apply_event(bg_progress(BackgroundAgentStatus::Running, 9))
                .unwrap();
            let agent = &s.background_agents[0];
            assert_eq!(agent.status, status, "{status:?} must not reopen");
            assert!(agent.ended_at.is_some(), "{status:?}");
            assert!(!s.has_active_background_agent(), "{status:?}");
        }
    }

    /// #4001: `turn_active` gates prompt dispatch and the queue drain, not
    /// just display, so `Stopped` clears it even while a sub-agent the turn
    /// spawned runs on. The busy display signal is the pair of flags.
    #[test]
    fn turn_active_tracks_only_the_main_turn() {
        let mut s = applied([prompt("go"), launched()]);
        assert!(s.turn_active && s.has_active_background_agent());

        s.apply_event(bg_completed(BackgroundAgentStatus::Completed))
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

        s.apply_event(bg_completed(BackgroundAgentStatus::Completed))
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
        s.apply_event(Event::SessionCleared).unwrap();
        assert_eq!(s.config_options.len(), 2);
        assert!(s.available_commands[0].accepts_input);
        assert_eq!(s.available_modes.len(), 1);
        assert_eq!(s.current_mode_id.as_deref(), Some("plan"));
        assert!(s.usage.is_none() && s.current_plan.is_none());

        s.apply_event(Event::ConfigOptionSwitchFailed {
            config_id: "effort".into(),
            value: "high".into(),
            reason: "unsupported".into(),
        })
        .unwrap();
        s.apply_event(switch_agent()).unwrap();
        assert!(s.config_options.is_empty() && s.config_option_switch_failed.is_none());
        assert!(s.available_commands.is_empty() && s.available_modes.is_empty());
        assert_eq!(s.current_mode_id, None);
        assert_eq!(s.agent.0, "codex");
    }
}

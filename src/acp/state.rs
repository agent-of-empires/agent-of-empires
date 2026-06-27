//! AcpState: the single-writer actor model for structured view session state.
//!
//! All mutations flow through `apply_event`. There is exactly one writer per
//! session. Worker-side notifications (`session/update`) and client-side
//! resolutions (approval taps) both become `Event` values that go through
//! `apply_event`. This eliminates the two-writer race condition that v3's
//! sketch had.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::approvals::{Approval, ApprovalDecision, Nonce};
use super::elicitations::{Elicitation, ElicitationAnswer, ElicitationOutcome};

/// Identifier for a structured view session. Distinct from `SessionId` in
/// `src/session/` because structured view sessions are a separate `SessionBackend`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AcpSessionId(pub String);

/// Which backend agent is running this session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentName(pub String);

/// One step of an agent-emitted plan.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// ACP `ToolKind` lowercased: `read` / `edit` / `delete` / `move` /
    /// `search` / `execute` / `think` / `fetch` / `switch_mode` / `other`.
    /// Lets the UI pick a per-tool renderer.
    #[serde(default)]
    pub kind: String,
    /// 16 KB cap applied at ingest, control chars stripped.
    pub args_preview: String,
    pub started_at: DateTime<Utc>,
    /// When the agent launches a sub-agent (Claude's Task tool) the
    /// adapter rides `_meta.claudeCode.parentToolUseId` along on the
    /// child tool calls. We thread it through here so the structured view can
    /// render sub-tasks under their parent Task instead of as a flat
    /// stream. None for top-level tool calls. See #1041.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    /// Populated when claude-agent-acp routes a session-start memory
    /// recall through the tool channel
    /// (`_meta.claudeCode.toolName == "memory_recall"`, upstream
    /// agentclientprotocol/claude-agent-acp#703 in v0.37.0). Carries
    /// the file paths the SDK loaded into the agent's context (recall
    /// mode) or the synthesized memory text (synthesize mode) so the
    /// structured view can render a dedicated card instead of treating it as a
    /// generic read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_recall: Option<MemoryRecall>,
    /// Structured file diffs the agent attached to this tool call via ACP
    /// `ToolCallContent::Diff`. Codex routes `apply_patch` edits through
    /// this channel (one entry per touched file) instead of the legacy
    /// `old_string`/`new_string` raw_input keys, so the structured view edit card
    /// reads the path and +/- preview from here when present and falls
    /// back to the args-preview shape otherwise. Text on each side is
    /// capped at ingest (see `acp_client`) so a large patch can't bloat the
    /// event store or WS frame. See #1721.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diffs: Vec<DiffPreview>,
}

/// Structured payload for a `memory_recall` tool call. `mode` mirrors
/// the adapter's `_meta.claudeCode.toolResponse.mode` field:
/// `"recall"` populates `paths` (one per loaded memory file);
/// `"synthesize"` populates `synthesized_text` with the SDK's
/// summarised reply. Either field may be empty when the adapter
/// reports the mode but no entries; the renderer falls back to the
/// title in that case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryRecall {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesized_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffPreview {
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// One renderable block of a tool call's completion payload, bridged from
/// an ACP `ToolCallContent` block. Carries the structured shape (image,
/// audio, resource) so the cockpit can render media on completion instead
/// of collapsing everything to text. The web card renders these richly;
/// the native TUI shows a textual placeholder for the non-text variants.
/// See #1818.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutputBlock {
    Text {
        text: String,
    },
    Image {
        mime_type: String,
        /// Base64-encoded bytes. Absent when the block referenced a `uri`
        /// only, or when the inline payload exceeded the size cap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    Audio {
        mime_type: String,
        /// Base64-encoded bytes. Absent when the inline payload exceeded
        /// the size cap (a text placeholder is emitted alongside).
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
        /// Inline text for a text resource. Absent for a binary (blob)
        /// resource, which carries `data` instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// Base64-encoded bytes for a binary (blob) resource, so the card
        /// can offer the payload as a download even without a fetchable uri.
        /// Absent for a text resource, or when the blob exceeded the size
        /// cap (the uri remains as a fallback).
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
    pub resets_at: DateTime<Utc>,
    pub kind: String,
}

/// Snapshot of the most recent ACP agent handoff. Stored on
/// `AcpState` so reload/replay reflects the active backend without
/// needing to walk the event log. Emitted by the `/acp/switch-agent`
/// path when a session moves from one ACP backend to another (e.g.
/// Claude -> Codex after a rate-limit). See #1282.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSwitchInfo {
    pub from: String,
    pub to: String,
    pub reason: String,
    pub switched_at: DateTime<Utc>,
}

/// Snapshot of the agent's last-reported context-window usage and
/// (optionally) cumulative session cost. Mirrors the ACP
/// `UsageUpdate` notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUsage {
    /// Tokens currently in context.
    pub used: u64,
    /// Total context window size in tokens.
    pub size: u64,
    /// Cumulative cost since session start, when the agent reports it.
    #[serde(default)]
    pub cost: Option<UsageCost>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageCost {
    pub amount: f64,
    /// ISO 4217 code (USD/EUR/...).
    pub currency: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionMode {
    Default,
    Plan,
    AcceptEdits,
    BypassPermissions,
}

/// One mode advertised by the agent. Mirrors ACP's `SessionMode`
/// shape: id is the canonical token (passed back via `set_mode`),
/// name is what the user sees, description is an optional tooltip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// One slash command advertised by the agent. Mirrors ACP's
/// `AvailableCommand` shape. `name` is the canonical token (sent back
/// to the agent as `/<name> <args>`); `description` is the human label
/// for the picker; `accepts_input` is true when the agent reports an
/// `Unstructured` input spec, signalling the command takes free-form
/// arguments after the name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub accepts_input: bool,
}

/// Semantic category for an ACP `SessionConfigOption`. Mirrors the
/// upstream schema's `SessionConfigOptionCategory` so the structured view UI
/// can pick the right widget per category (model dropdown, effort
/// segmented control, etc.) without hardcoding option ids. Unknown
/// categories fall through to `Other(String)` so the broadcast frame
/// stays forward-compatible with new adapter categories.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigOptionCategory {
    Mode,
    Model,
    ThoughtLevel,
    #[serde(untagged)]
    Other(String),
}

/// One choice in a `Select`-kind `SessionConfigOption`. `value` is the
/// token the agent expects back via `session/set_config_option`;
/// `name` is the user-facing label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigOptionChoice {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Acp's view of a single ACP `SessionConfigOption`. Built from
/// `SessionUpdate::ConfigOptionUpdate` notifications; the adapter
/// resends the full snapshot whenever any selector changes, so the
/// structured view treats each `ConfigOptionsUpdated` event as a full
/// replacement of the previous list.
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

/// Carried by `Event::ConfigOptionSwitchFailed` and stored on
/// `AcpState.config_option_switch_failed` so the UI can render a
/// non-blocking notice when the adapter rejects a
/// `session/set_config_option` call. Auto-clears when a later
/// `ConfigOptionsUpdated` snapshot reports the originally-requested
/// value as current, or on `AgentSwitched`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigOptionSwitchFailure {
    pub config_id: String,
    pub value: String,
    pub reason: String,
}

/// Structured detail about why aoe refused to enter the session after
/// the ACP `initialize` handshake completed. Distinct from the runtime
/// `Stopped` taxonomy: a startup error means the session never reached
/// the Running state. The structured view UI short-circuits its normal render
/// when this field is populated and shows a dedicated screen with the
/// exact remediation command. Populated by the per-adapter compatibility
/// check (see `src/acp/agent_compat.rs`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StartupErrorDetail {
    IncompatibleAgentVersion {
        package_name: String,
        installed: String,
        required: String,
        install_command: String,
        /// True when the web "Update & restart" action can install this
        /// agent via `npm install -g`; false means the manual hint only.
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

/// Lifecycle status of an async background sub-agent. `Completed` is the
/// only "finished cleanly" state and is set ONLY on an `end_turn` in the
/// transcript; idle/missing-file/parse states are reported honestly
/// rather than faked as done. See the background-agent tailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundAgentStatus {
    /// Transcript is being written; the agent is working.
    Running,
    /// No transcript growth for the idle window; the agent may be
    /// blocked, rate-limited, or wedged. Never flips to `Completed`.
    Stalled,
    /// Saw the terminal `end_turn` assistant message. The work is done.
    Completed,
    /// The parent session ended before the agent finished; we stopped
    /// tracking it. Not a success, not a failure.
    Detached,
    /// Transcript could not be read or its format was unrecognized.
    Error,
}

/// One tool call a background sub-agent made, parsed from its transcript
/// so the panel can list individual reads / bashes / greps like the main
/// output. Mirrors the web wire type in `web/src/lib/acpTypes.ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundAgentTool {
    /// Tool name, e.g. "Read", "Bash", "Grep".
    pub name: String,
    /// Short label from the tool input (command / file path / pattern).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Outcome from the matching tool_result: `None` while running,
    /// `Some(true)` succeeded, `Some(false)` errored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
}

/// One async background sub-agent, built up from `BackgroundAgent*`
/// events. Mirrors the web wire type in `web/src/lib/acpTypes.ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundAgentRecord {
    pub agent_id: String,
    /// The parent `Task` tool call that launched this agent, so the
    /// inline tool card can link to the panel entry.
    pub tool_call_id: String,
    pub description: String,
    pub prompt: String,
    pub model: String,
    pub status: BackgroundAgentStatus,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    /// Number of tool calls the agent has made so far (from its transcript).
    #[serde(default)]
    pub tool_count: u32,
    /// The agent's individual tool calls, in order, so the panel can show
    /// each read / bash / grep like the main output.
    #[serde(default)]
    pub tools: Vec<BackgroundAgentTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<String>,
    /// Short preview of the agent's most recent assistant text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_text: Option<String>,
    /// Final result text (the terminal assistant message), set on completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Non-fatal note shown in the panel, e.g. transcript format not recognized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpState {
    pub session_id: AcpSessionId,
    pub agent: AgentName,
    pub model: Option<String>,
    pub mode: SessionMode,

    pub current_plan: Option<Plan>,
    pub todos: Vec<Todo>,
    pub in_flight_tool: Option<ToolCall>,
    pub pending_approvals: Vec<Approval>,
    /// Pending `AskUserQuestion` elicitations awaiting a user answer.
    /// Parallel to `pending_approvals`; cleared on resolution, session
    /// reset, or clear. See `Event::ElicitationRequested`.
    #[serde(default)]
    pub pending_elicitations: Vec<Elicitation>,
    pub recent_diffs: Vec<DiffPreview>,
    pub thinking: Option<ThinkingSignal>,
    pub rate_limit: Option<RateLimitInfo>,
    /// Last-known context-window usage from the agent's most recent
    /// `UsageUpdate`. None until the agent emits one.
    #[serde(default)]
    pub usage: Option<SessionUsage>,
    /// Slash commands the agent advertised in its most recent
    /// `AvailableCommandsUpdate`. Empty until the agent emits one. Used
    /// by the composer's `/` picker so users see real plugin/skill/MCP
    /// commands instead of a hard-coded placeholder list.
    #[serde(default)]
    pub available_commands: Vec<AvailableCommand>,
    /// Most recent `AgentSwitched` snapshot. Used by the UI to render a
    /// transcript divider (e.g. "Switched claude -> codex due to
    /// rate_limit") and by the post-switch context-primer fetch. None
    /// until the session has ever moved backends. See #1282.
    #[serde(default)]
    pub last_agent_switch: Option<AgentSwitchInfo>,
    /// Structured startup error from the per-adapter compatibility
    /// check. When `Some`, the structured view UI replaces its normal session
    /// view with a dedicated remediation screen. `None` for healthy
    /// sessions and for legacy `AgentStartupError` failures (those
    /// only carry a free-form message; see `Event::AgentStartupError`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_error: Option<StartupErrorDetail>,
    /// Full snapshot of the per-session selectors the adapter
    /// advertises (model, reasoning effort, mode, future categories).
    /// Mirrors the most recent ACP `ConfigOptionUpdate` notification.
    /// Empty when the adapter does not advertise any config options
    /// (older adapters, non-Claude backends). See #1403.
    #[serde(default)]
    pub config_options: Vec<ConfigOptionDescriptor>,
    /// Non-blocking notice for the most recent `session/set_config_option`
    /// rejection. Cleared automatically by the next snapshot whose
    /// matching `config_id` carries the originally-requested value, or
    /// on `AgentSwitched`. Mirrors `ModeSwitchFailed` (#1233).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_option_switch_failed: Option<ConfigOptionSwitchFailure>,
    /// Async sub-agents (Claude `Task` with isAsync) launched this
    /// session. The parent ACP stream only carries each launch; the
    /// daemon tails each agent's on-disk transcript and emits
    /// `BackgroundAgent*` events that build this list. Drives the web
    /// "Background agents" panel and the inline Task-card linkage.
    #[serde(default)]
    pub background_agents: Vec<BackgroundAgentRecord>,

    pub last_seq: u64,
    pub updated_at: DateTime<Utc>,
}

impl AcpState {
    /// Bounded ring of recent diffs. Keep the last 16 to keep state size
    /// bounded; the full diff history lives in the replay buffer.
    const MAX_RECENT_DIFFS: usize = 16;

    pub fn new(session_id: AcpSessionId, agent: AgentName, model: Option<String>) -> Self {
        Self {
            session_id,
            agent,
            model,
            mode: SessionMode::Default,
            current_plan: None,
            todos: Vec::new(),
            in_flight_tool: None,
            pending_approvals: Vec::new(),
            pending_elicitations: Vec::new(),
            recent_diffs: Vec::new(),
            thinking: None,
            rate_limit: None,
            usage: None,
            available_commands: Vec::new(),
            last_agent_switch: None,
            startup_error: None,
            config_options: Vec::new(),
            config_option_switch_failed: None,
            background_agents: Vec::new(),
            last_seq: 0,
            updated_at: Utc::now(),
        }
    }
}

/// Single writer entry point. Every mutation goes through here so the
/// state has exactly one source of truth and `last_seq` stays monotonic.
#[derive(Debug, Error)]
pub enum StateError {
    #[error("approval nonce {0:?} did not match any pending approval")]
    UnknownApprovalNonce(Nonce),
    #[error("approval nonce {0:?} already resolved")]
    ApprovalAlreadyResolved(Nonce),
}

/// A single user-authored diff-line review comment, carried verbatim
/// in `Event::UserDiffCommentsPrompt` so the structured view transcript can
/// re-render the rich review card on replay without parsing the
/// assembled markdown. Field names mirror the frontend `DiffComment`
/// type (`web/src/components/diff/comments/types.ts`) one-for-one; the
/// server never interprets these, it only stores and replays them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffComment {
    pub id: String,
    /// Workspace member name. Absent for single-repo sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    pub file_path: String,
    /// `"old"` or `"new"`. Stored as a string so an unrecognised side
    /// from a future frontend never fails replay of the whole log.
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

/// Which ACP `ContentBlock` an attachment maps to. The string form
/// (`"image"` / `"audio"` / `"resource"`) is the wire contract shared
/// with the web composer and the prompt-request DTO in `protocol.rs`,
/// so renaming a variant breaks the build on both sides rather than
/// silently dropping attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptAttachmentKind {
    Image,
    Audio,
    Resource,
}

impl PromptAttachmentKind {
    /// Stable lowercase tag, matching the serde wire form. Used by the
    /// attachment store to persist the kind as a TEXT column.
    pub fn as_str(self) -> &'static str {
        match self {
            PromptAttachmentKind::Image => "image",
            PromptAttachmentKind::Audio => "audio",
            PromptAttachmentKind::Resource => "resource",
        }
    }
}

/// Replay-side view of one prompt attachment. Carries metadata only,
/// never the bytes: the decoded blob lives in the `acp_attachments`
/// table keyed by `(session_id, id)` and is fetched lazily over
/// `GET /acp/attachments/{id}`. Keeping bytes out of the event log
/// is what stops `event_json` (and every WS replay frame) from bloating
/// to megabytes per screenshot. See #1000 / #965.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptAttachmentRef {
    pub id: String,
    pub kind: PromptAttachmentKind,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Decoded byte length, for the UI to show a size hint without
    /// fetching the blob.
    pub size: u64,
}

/// Discriminated union of state mutations. ACP `session/update`
/// notifications become specific variants; client approval taps also
/// become variants and flow through the same path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    PlanUpdated {
        plan: Plan,
    },
    TodoListUpdated {
        todos: Vec<Todo>,
    },
    /// Legacy event for agent-pushed ACP `session_info_update` titles. Kept so
    /// persisted event logs from versions that emitted it still deserialize.
    /// New Claude ACP title pushes are ignored; AoE owns automatic renaming via
    /// `session::smart_rename`.
    SessionTitleSuggested {
        title: String,
    },
    ToolCallStarted {
        tool_call: ToolCall,
    },
    ToolCallCompleted {
        tool_call_id: String,
        is_error: bool,
        /// Final textual output extracted from ACP `ToolCallUpdate.fields.content`
        /// (concat of all `ToolCallContent::Content(Text(_))` blocks). Empty
        /// when the agent emits no content blocks on completion. Renderers
        /// fall back to a status word ("completed" / "tool failed") when this
        /// is empty so cards still convey state.
        #[serde(default)]
        content: String,
        /// Structured completion payload bridged from the ACP
        /// `ToolCallContent` blocks (images, audio, resource links/contents,
        /// plus text). Lets the card render media that arrives only at
        /// completion instead of collapsing it to the status word. Empty for
        /// text-only completions (the `content` field already carries that)
        /// and for events persisted before this field landed. See #1818.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        output: Vec<ToolOutputBlock>,
        /// Server-side wall-clock time the completion frame was minted.
        /// Carried on the event so the frontend reducer can stamp the
        /// matching `tool_complete` activity row with the REAL
        /// completion time rather than `new Date()` at replay time;
        /// without this, page-reload after a long delay made every
        /// completed tool's duration count from "now", inflating the
        /// label from seconds to minutes/hours. Events persisted
        /// before this field landed default to "now" on deserialise
        /// (serde calls the function), so the durations of pre-fix
        /// events stay imprecise; new events are accurate end-to-end.
        #[serde(default = "chrono::Utc::now")]
        completed_at: DateTime<Utc>,
        /// True when this completion is the synchronous launch of an
        /// asynchronous sub-agent (Claude's `Task` tool with
        /// `isAsync: true`): the visible tool call completes immediately
        /// with the SDK marker `Async agent launched successfully` while
        /// the real sub-agent runs off-protocol and never reports back on
        /// this stream. Renderers use this to draw a neutral "runs in
        /// background" sub-agent card instead of a finished tool card, and
        /// to suppress the marker body (which carries an internal agent id
        /// the SDK tells us not to surface). Defaults false for events
        /// persisted before this field landed. See `OffProtocolWorkKind`.
        #[serde(default)]
        async_subagent: bool,
    },
    /// Streaming tool output. Some agents emit `ToolCallUpdate` notifications
    /// with `status != Completed` but populated `fields.content` to stream
    /// stdout/stderr while the call is still running. Each event carries the
    /// LATEST full content snapshot for that call (per ACP, the content
    /// field is a replacement, not an append). Reducer buffers it keyed by
    /// tool_call_id; on completion the buffer is used if the final update
    /// shipped no content of its own.
    ToolCallContent {
        tool_call_id: String,
        content: String,
    },
    /// Late-arriving title or raw_input for a tool call. Some agents
    /// (Claude's claude-agent-acp among them) emit the initial
    /// `tool_call` notification with an empty `raw_input` and only fill
    /// in the actual inputs in a follow-up `ToolCallUpdate`. Without
    /// this, bash cards render `$ Terminal` instead of the command and
    /// edit cards lose their target path. The reducer locates the
    /// matching tool_start row by id and overwrites its name/args.
    ToolCallUpdated {
        tool_call_id: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        args_preview: Option<String>,
        /// Re-stamps the tool's start time. Set when the agent reports
        /// `ToolCallStatus::InProgress`; claude-agent-acp emits the
        /// initial `tool_call` notification eagerly (often well before
        /// the underlying command actually starts running), so the
        /// duration label (#1060) would otherwise count adapter
        /// scheduling time as part of the tool's runtime. Treating
        /// "InProgress" as the real start gives an accurate elapsed
        /// time on completion.
        #[serde(default)]
        started_at: Option<DateTime<Utc>>,
        /// Structured diffs carried on a late `ToolCallUpdate.fields.content`
        /// frame (Codex emits `apply_patch` diffs on the in-progress and
        /// completion updates, not only the initial `tool_call`). `Some`
        /// REPLACES the tool's diff list wholesale (per ACP, content is a
        /// replacement, not an append); `None` leaves any diffs from the
        /// initial frame untouched so a text-only update can't erase them.
        /// See #1721.
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
    /// Agent asked the user a structured question (the ACP
    /// `AskUserQuestion` tool, surfaced as a form-mode
    /// `elicitation/create`). The card stays until an
    /// `ElicitationResolved` with the same nonce arrives.
    ElicitationRequested {
        elicitation: Elicitation,
    },
    /// An elicitation was answered, skipped, cancelled, or torn down. The
    /// reducer drops the matching pending card. `outcome` records how it
    /// ended for replay/debugging. `answers` carries the user's submitted
    /// answers (display-ready, in form order) so the transcript can show
    /// what was picked after the card closes; empty for skip/cancel/teardown
    /// and for events stored before #2209.
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
    /// Opt-in auto-resume breadcrumb. Published by the reconciler (not the
    /// agent) when a session parked on `Stopped { reason: "rate_limited" }`
    /// crosses its reset deadline and `acp.rate_limit_auto_resume` is
    /// enabled, just before the same worker is respawned. Carries the
    /// `resets_at` that gated the resume so the timeline can show why the
    /// worker came back, and so the web reducer can clear the rate-limit
    /// lock and drain any queued prompt. See #1722.
    RateLimitAutoResumed {
        resets_at: DateTime<Utc>,
    },
    /// Agent-reported context-window usage. Comes from ACP
    /// `SessionUpdate::UsageUpdate` (gated on the
    /// `unstable_session_usage` schema feature). Latest snapshot wins;
    /// the agent typically resends after each turn.
    UsageUpdated {
        usage: SessionUsage,
    },
    ModeChanged {
        mode: SessionMode,
    },
    /// Real ACP-advertised modes. Emitted once when the agent
    /// announces them (in `NewSessionResponse.modes`) so the UI can
    /// render the actual modes the agent supports rather than the
    /// hard-coded four. The id is the token that goes back via
    /// `session/set_mode`.
    ModesAvailable {
        current_mode_id: String,
        modes: Vec<ModeInfo>,
    },
    /// Agent-driven mode switch. Comes from ACP
    /// `SessionUpdate::CurrentModeUpdate`; UI swaps `current_mode_id`.
    CurrentModeChanged {
        current_mode_id: String,
    },
    /// `session/set_mode` round-trip rejected by the adapter. Fired when
    /// the structured view asked for a mode the adapter does not advertise
    /// (claude-agent-acp gates `bypassPermissions` on `ALLOW_BYPASS`, so
    /// a YOLO-driven post-spawn `set_mode("bypassPermissions")` lands
    /// here when the env var is unset). UI renders a non-blocking notice
    /// so the user knows their requested mode did not take effect; the
    /// session keeps whatever mode the adapter last reported. See #1233.
    ModeSwitchFailed {
        mode_id: String,
        reason: String,
    },
    /// Full snapshot of the slash commands the agent advertises. Comes
    /// from ACP `SessionUpdate::AvailableCommandsUpdate`. Replaces the
    /// previous list (the agent re-broadcasts the full set whenever it
    /// changes; e.g. after plugin enable/disable).
    AvailableCommandsUpdated {
        commands: Vec<AvailableCommand>,
    },
    /// Full snapshot of the per-session selectors the adapter
    /// advertises. Comes from ACP `SessionUpdate::ConfigOptionUpdate`
    /// (stabilised in claude-agent-acp v0.37.0). The adapter resends
    /// the full set whenever any selector changes; the reducer
    /// replaces (not merges) the prior `config_options`. Also
    /// auto-clears `config_option_switch_failed` when the snapshot's
    /// matching config_id current_value equals the previously-failed
    /// value. See #1403.
    ConfigOptionsUpdated {
        options: Vec<ConfigOptionDescriptor>,
    },
    /// `session/set_config_option` round-trip rejected by the adapter.
    /// UI renders a non-blocking notice and the session keeps whatever
    /// value the adapter last reported. Mirrors `ModeSwitchFailed`
    /// (#1233). Auto-dismisses on the next confirming
    /// `ConfigOptionsUpdated` snapshot or on `AgentSwitched`.
    ConfigOptionSwitchFailed {
        config_id: String,
        value: String,
        reason: String,
    },
    /// Passthrough for an ACP `session/update` payload that we have not yet
    /// finished mapping to a typed variant. Useful while the structured view's
    /// typed schema is still expanding to cover every ACP update kind.
    /// Carries the raw JSON so UI clients can render best-effort.
    RawAgentUpdate {
        payload: serde_json::Value,
    },
    /// An async sub-agent (Claude `Task` with isAsync) was launched. The
    /// parent ACP stream carries only this launch; the daemon then tails
    /// the agent's on-disk transcript and emits `BackgroundAgentProgress`
    /// / `BackgroundAgentCompleted`. See the background-agent tailer.
    BackgroundAgentLaunched {
        agent_id: String,
        tool_call_id: String,
        description: String,
        prompt: String,
        model: String,
        /// Local transcript path the daemon tails. Never serialized (a
        /// host fs path, useless and mildly sensitive to clients); read
        /// by the notification handler to spawn the tailer, then dropped.
        #[serde(default, skip_serializing)]
        output_file: String,
        started_at: DateTime<Utc>,
    },
    /// Throttled snapshot of a running background sub-agent's transcript
    /// (tool count + last action). Persisted so a mid-run reload still
    /// sees in-flight agents, but coalesced so the event log stays bounded.
    BackgroundAgentProgress {
        agent_id: String,
        status: BackgroundAgentStatus,
        tool_count: u32,
        /// Full ordered tool list so far (coalesced into the snapshot, not
        /// one event per tool). Replaces the record's list wholesale.
        #[serde(default)]
        tools: Vec<BackgroundAgentTool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_tool: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_text: Option<String>,
        at: DateTime<Utc>,
    },
    /// Terminal state for a background sub-agent: `Completed` (saw
    /// `end_turn`), `Stalled`, `Detached`, or `Error`.
    BackgroundAgentCompleted {
        agent_id: String,
        status: BackgroundAgentStatus,
        /// Final ordered tool list. Replaces the record's list.
        #[serde(default)]
        tools: Vec<BackgroundAgentTool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
        ended_at: DateTime<Utc>,
    },
    /// A prompt reached the adapter, but the adapter-side runtime failed
    /// before any assistant transcript or tool event was emitted. Used
    /// for recoverable turn failures, distinct from startup/handshake
    /// errors that block the whole structured-view session. See #2426.
    PromptRuntimeError {
        message: String,
    },
    /// An assistant message chunk (text). In ACP this comes as an
    /// `agent_message_chunk` session update.
    AgentMessageChunk {
        text: String,
    },
    /// A cancel was requested for the in-flight turn: aoe sent the ACP
    /// `session/cancel` notification and armed the escalation watchdog.
    /// The turn is NOT over yet (no `Stopped`); this lets the UI show a
    /// "Stopping..." state and reveal a force-stop affordance instead of
    /// a silent spinner. `escalates_at` is when the watchdog will SIGTERM
    /// the worker if the agent keeps ignoring the cancel, so the UI can
    /// show an honest countdown without depending on a local timer for
    /// correctness. Emitted once per turn on the first cancel. See #1727.
    CancelRequested {
        escalates_at: DateTime<Utc>,
    },
    /// Final stop signal from the agent. Carries an opaque reason string
    /// so the UI can render "completed" / "ended early" / "cancelled".
    Stopped {
        reason: String,
    },
    /// The agent process failed to spawn or never completed its
    /// `initialize` handshake. Surfaced through the broadcast so the
    /// React structured view can show a remediation hint instead of staring at
    /// an empty conversation.
    AgentStartupError {
        message: String,
    },
    /// The ACP `initialize` handshake completed but the adapter failed
    /// the per-adapter compatibility policy. Structured payload so the
    /// structured view UI can render an actionable remediation screen with the
    /// exact install command. Emitted by the connection task right
    /// before it closes; the connection drops, the child is killed, and
    /// a parallel `AgentStartupError { message }` is published so legacy
    /// status-derivation paths still flip the session into Error state.
    /// See `src/acp/agent_compat.rs`.
    IncompatibleAgent {
        detail: StartupErrorDetail,
    },
    /// Echo of a user-submitted prompt. Published synchronously by the
    /// `POST /acp/prompt` handler before the text is forwarded to
    /// the agent, so the replay buffer (and the on-disk event store)
    /// captures the user's side of the conversation. Without this,
    /// reload/session-switch reconstructs only the agent's chunks and
    /// every turn collapses into one assistant blob.
    UserPromptSent {
        text: String,
        /// Attachments the user sent alongside the text (images, audio,
        /// embedded resources). Metadata only; bytes live in the
        /// `acp_attachments` store. `#[serde(default)]` keeps
        /// pre-attachment events on disk deserialising as text-only, so
        /// no migration is needed. See #1000 / #965.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<PromptAttachmentRef>,
    },
    /// The agent's prompt capabilities, captured from the ACP
    /// `initialize` response right after the handshake (and re-emitted
    /// on every connect, since `initialize` runs in both Fresh and
    /// Resume modes). Persisted + replayed so the web composer can gate
    /// the attachment button on the current agent without a round-trip,
    /// and so a reconnecting client reconstructs the gate from history.
    /// The server prompt handler reads the latest one to reject
    /// attachments an agent cannot accept. See #1000.
    PromptCapabilities {
        image: bool,
        audio: bool,
        embedded_context: bool,
    },
    /// Echo of a "Send diff comments" submission, published by the
    /// `POST /acp/prompt/diff-comments` handler before
    /// `assembled_markdown` is forwarded to the agent. The agent only
    /// ever sees `assembled_markdown` (no sentinel); the structured
    /// fields exist so the structured view transcript re-renders the rich
    /// `DiffCommentsUserCard` on replay without parsing the markdown.
    /// `intro`/`outro` are the effective values the user approved in the
    /// dialog (trimmed intro, defaulted outro), so replay matches what
    /// the agent received. Replaces the legacy
    /// `<!-- aoe:diff-comments:v1 ... -->` sentinel carried inside an
    /// ordinary `UserPromptSent`; older persisted sentinel events keep
    /// rendering via the frontend decode fallback.
    #[serde(rename_all = "camelCase")]
    UserDiffCommentsPrompt {
        intro: String,
        outro: String,
        is_multi_repo: bool,
        comments: Vec<DiffComment>,
        assembled_markdown: String,
    },
    /// A user prompt arrived at the daemon while another `session/prompt`
    /// was still in flight. The daemon refused to forward it (claude-agent-acp
    /// serializes prompts internally and a second concurrent prompt would
    /// race the pending one). Carries the rejected text so the UI can
    /// render a Retry pill near the composer. The text was already
    /// persisted as `UserPromptSent` upstream of this rejection by the
    /// `/acp/prompt` handler, so this event does not introduce new
    /// PII exposure relative to the existing transcript. Reason is an
    /// opaque tag for forward extensibility; today only `"agent_busy"`
    /// is used. See #1196.
    PromptRejected {
        reason: String,
        text: String,
    },
    /// Agent-assigned ACP session id from a successful `session/new`.
    /// Server-side listener catches this and persists the id on
    /// `Instance.acp_session_id` so the next spawn can call
    /// `session/load` and the model retains context across `aoe serve`
    /// restarts. Not emitted on `session/load` success (id unchanged).
    AcpSessionAssigned {
        acp_session_id: String,
    },
    /// `session/load` failed and we fell back to `session/new`. The
    /// agent's stored transcript is gone (or the id was never valid),
    /// so the model starts with no context. UI uses this to render a
    /// muted notice and clear the now-stale token-usage hint; the
    /// server-side listener clears `Instance.acp_session_id`
    /// before the new id arrives via `AcpSessionAssigned`.
    SessionContextReset {
        reason: String,
    },
    /// The agent invoked the Claude SDK's `ScheduleWakeup` tool. The
    /// session will sit idle until `at`, then a new turn fires. Emitted
    /// from `acp_client::map_update_to_events` on `ToolCallStarted` for
    /// `ScheduleWakeup` so the sidebar can flip to a "scheduled" badge
    /// plus countdown without subscribing to the structured view WS. Considered
    /// pending until the next `UserPromptSent` lands, which is what
    /// /loop's self-firing emits when the wake actually triggers. See
    /// #1091.
    WakeupScheduled {
        at: DateTime<Utc>,
        reason: Option<String>,
    },
    /// The agent armed the Claude SDK's `Monitor` tool: a background watch
    /// that streams events and re-invokes the agent off-protocol. Unlike
    /// `ScheduleWakeup` it has no fixed wake time, so there is no countdown,
    /// just a "monitoring" badge. The tool call is fire-and-forget (it
    /// completes immediately while the watch keeps running), so the turn
    /// ends and the session sits Idle while the monitor is still armed.
    /// Emitted from `acp_client::map_update_to_events` so the sidebar can
    /// flag the session without subscribing to the structured view WS.
    /// Considered active until the next `UserPromptSent`: a monitor firing
    /// re-invokes the agent with activity but never a `UserPromptSent`, so
    /// the badge persists across re-fires and clears only when the user
    /// takes over.
    MonitorArmed {
        description: Option<String>,
    },
    /// User invoked `/clear` (claude-agent-acp's reset-conversation
    /// slash command). The adapter rotates its internal session so the
    /// model truly forgets earlier turns; aoe's transcript is now a
    /// stale historical artifact. Reducer drops session-scoped
    /// capabilities (`availableCommands`, `availableModes`, `plan`,
    /// `mode`) and cancels any open approvals; UI collapses rows above
    /// the divider behind a disclosure. Distinct from
    /// `SessionContextReset` (which fires only on `session/load`
    /// failure now) and `ConversationCompacted` because the
    /// user-experience contract differs: cleared is "the model has
    /// forgotten", reset is "the model has empty context", compacted
    /// is "the model has a summary". See #1101.
    SessionCleared,
    /// `/compact` cycle completed: the model's context window has been
    /// replaced with a summary of the prior turns. The model still
    /// has continuity through the summary, so unlike
    /// `SessionContextReset` there is no recovery to offer; the
    /// reducer drops the now-stale usage snapshot and the UI renders
    /// an inline divider but does NOT surface the context-primer
    /// banner. See #1109.
    ConversationCompacted,
    /// The session's ACP backend was switched from one agent to
    /// another (e.g. Claude -> Codex after a rate-limit). Emitted by
    /// the `/acp/switch-agent` endpoint AFTER the new worker has
    /// spawned and the instance's `agent_name` is persisted. The
    /// reducer drops all agent-specific transient state (rate-limit
    /// banner, in-flight tool, thinking, pending approvals, usage,
    /// available commands, modes) since none of it carries over to a
    /// different backend. See #1282.
    AgentSwitched {
        from: String,
        to: String,
        reason: String,
    },
}

impl AcpState {
    /// Apply a single event. Returns the new `last_seq` on success.
    pub fn apply_event(&mut self, event: Event) -> Result<u64, StateError> {
        match event {
            Event::PlanUpdated { plan } => self.current_plan = Some(plan),
            Event::TodoListUpdated { todos } => self.todos = todos,
            // Session title lives on `Instance`, not `AcpState`; the daemon's
            // `acp_event_listener` applies it. No transcript state to mutate.
            Event::SessionTitleSuggested { .. } => {}
            Event::ToolCallStarted { tool_call } => self.in_flight_tool = Some(tool_call),
            Event::ToolCallCompleted { tool_call_id, .. } => {
                if self
                    .in_flight_tool
                    .as_ref()
                    .map(|t| t.id == tool_call_id)
                    .unwrap_or(false)
                {
                    self.in_flight_tool = None;
                }
            }
            Event::ToolCallContent { .. } => {}
            Event::ToolCallUpdated {
                tool_call_id,
                title,
                args_preview,
                started_at,
                diffs,
            } => {
                if let Some(tool) = self.in_flight_tool.as_mut() {
                    if tool.id == tool_call_id {
                        if let Some(t) = title {
                            tool.name = t;
                        }
                        if let Some(a) = args_preview {
                            tool.args_preview = a;
                        }
                        if let Some(t) = started_at {
                            tool.started_at = t;
                        }
                        if let Some(d) = diffs {
                            tool.diffs = d;
                        }
                    }
                }
            }
            Event::ApprovalRequested { approval } => self.pending_approvals.push(approval),
            Event::ApprovalResolved { ref nonce, .. } => {
                let pos = self
                    .pending_approvals
                    .iter()
                    .position(|a| a.nonce == *nonce)
                    .ok_or_else(|| StateError::UnknownApprovalNonce(nonce.clone()))?;
                let resolved = self.pending_approvals.remove(pos);
                if resolved.resolved.is_some() {
                    return Err(StateError::ApprovalAlreadyResolved(nonce.clone()));
                }
            }
            Event::ElicitationRequested { elicitation } => {
                self.pending_elicitations.push(elicitation)
            }
            // Lenient on the nonce: a resolved/torn-down elicitation can be
            // re-broadcast (cancel-on-teardown racing a user POST), so a
            // missing nonce is a harmless no-op rather than a hard error.
            Event::ElicitationResolved { ref nonce, .. } => {
                self.pending_elicitations.retain(|e| e.nonce != *nonce);
            }
            Event::DiffEmitted { diff } => {
                self.recent_diffs.push(diff);
                while self.recent_diffs.len() > Self::MAX_RECENT_DIFFS {
                    self.recent_diffs.remove(0);
                }
            }
            Event::ThinkingStarted => {
                self.thinking = Some(ThinkingSignal {
                    started_at: Utc::now(),
                });
            }
            Event::ThinkingEnded => self.thinking = None,
            Event::RateLimit { info } => self.rate_limit = Some(info),
            // Auto-resume fired: the park is over and a fresh worker is
            // being respawned. Clear the rate-limit snapshot so a client
            // seeding state from the store (or the persistent reducer)
            // doesn't keep showing the parked banner after the resume.
            Event::RateLimitAutoResumed { .. } => self.rate_limit = None,
            Event::UsageUpdated { usage } => self.usage = Some(usage),
            Event::ModeChanged { mode } => self.mode = mode,
            // ModesAvailable + CurrentModeChanged carry the real ACP-
            // advertised modes. The structured view's persistent state doesn't
            // track them yet (the UI stores them in the broadcast
            // replay), so this is just a no-op that bumps seq.
            Event::ModesAvailable { .. } => {}
            Event::CurrentModeChanged { .. } => {}
            Event::ModeSwitchFailed { .. } => {}
            Event::AvailableCommandsUpdated { commands } => {
                self.available_commands = commands;
            }
            Event::ConfigOptionsUpdated { options } => {
                // Auto-dismiss a stale switch-failed notice when this
                // snapshot reports the originally-requested value as
                // current (the user retried and won, or the adapter
                // applied the value asynchronously). Without this the
                // notice would linger until the user dismissed it.
                if let Some(failure) = self.config_option_switch_failed.as_ref() {
                    let confirmed = options
                        .iter()
                        .find(|opt| opt.id == failure.config_id)
                        .map(|opt| opt.current_value == failure.value)
                        .unwrap_or(false);
                    if confirmed {
                        self.config_option_switch_failed = None;
                    }
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
            // The next four variants don't directly mutate persistent
            // AcpState fields (yet); they bump seq/updated_at so
            // clients see them in the replay buffer and know the session
            // made progress.
            Event::RawAgentUpdate { .. } => {}
            Event::PromptRuntimeError { .. } => {}
            Event::AgentMessageChunk { .. } => {}
            // No in-memory mutation: the turn is still active (turnActive
            // stays true until a real `Stopped`). The reducer/UI derive the
            // "Stopping..." state from the broadcast/replayed event. Bumps
            // seq so the WS replay surfaces it to live clients. See #1727.
            Event::CancelRequested { .. } => {}
            Event::Stopped { .. } => {}
            Event::AgentStartupError { .. } => {}
            Event::IncompatibleAgent { detail } => {
                self.startup_error = Some(detail);
            }
            Event::UserPromptSent { .. } => {}
            // Like UserPromptSent, the diff-comments prompt doesn't mutate
            // persistent AcpState; it bumps seq so the replay buffer
            // and on-disk store capture the user's side of the turn.
            Event::UserDiffCommentsPrompt { .. } => {}
            // Surfaced to the web composer via replay and read by the
            // server prompt handler from the event store; no persistent
            // AcpState field consumes it, so this arm only bumps
            // seq/updated_at like the streaming events above.
            Event::PromptCapabilities { .. } => {}
            Event::AcpSessionAssigned { .. } => {
                // A fresh agent that passed the compatibility check
                // has come online; heal any sticky startup error so a
                // post-upgrade respawn unblocks the UI without a hard
                // reload. Mirrors the frontend reducer's
                // `incompatibleAgent = null` clear on the same event.
                self.startup_error = None;
            }
            Event::SessionContextReset { .. } => {
                // Agent's stored context is gone; clear the cached
                // usage snapshot so the composer footer doesn't keep
                // showing the old "75k / 200k" until the new session
                // emits its first UsageUpdate.
                self.usage = None;
            }
            Event::SessionCleared => {
                // /clear truly wipes the model's memory. Drop
                // session-scoped capability caches and the usage
                // snapshot so the UI doesn't keep showing stale data
                // referencing a conversation the model has forgotten.
                self.usage = None;
                self.available_commands = Vec::new();
                self.current_plan = None;
                self.mode = SessionMode::Default;
                self.pending_approvals = Vec::new();
                self.pending_elicitations = Vec::new();
            }
            Event::ConversationCompacted => {
                // /compact replaces the model's context with a summary
                // of the prior turns. The usage snapshot for the old
                // raw turns no longer matches what the model holds;
                // clear it so the next UsageUpdate seeds the new
                // (compacted) value. Plan/mode/commands persist:
                // unlike /clear, the model still has continuity here.
                self.usage = None;
            }
            // Persistent state for "scheduled wakeup" lives in the
            // event log (queried by the REST endpoint per #1091); no
            // in-memory mirror needed yet. Bumps seq so the WS replay
            // surfaces it to live clients.
            Event::WakeupScheduled { .. } => {}
            // Like WakeupScheduled, the "monitor armed" state lives in the
            // event log (queried by the REST endpoint); no in-memory mirror.
            // Bumps seq so the WS replay surfaces it to live clients.
            Event::MonitorArmed { .. } => {}
            // Rejected follow-up prompt while another prompt was in flight.
            // No durable in-memory mutation; the reducer surfaces a Retry
            // pill from the broadcast frame and the event_store entry
            // carries the historical record. See #1196.
            Event::PromptRejected { .. } => {}
            Event::AgentSwitched { from, to, reason } => {
                // The new backend has no knowledge of the prior agent's
                // session state. Drop everything tied to the previous
                // model/process so the UI doesn't render Claude's usage
                // bar, in-flight tool card, or mode pills while talking
                // to Codex. The transcript itself stays intact in the
                // event log; the visible history is regenerated from
                // replay on next reload.
                self.agent = AgentName(to.clone());
                self.rate_limit = None;
                self.in_flight_tool = None;
                self.thinking = None;
                self.pending_approvals = Vec::new();
                self.pending_elicitations = Vec::new();
                self.usage = None;
                self.available_commands = Vec::new();
                self.current_plan = None;
                self.mode = SessionMode::Default;
                // Per-adapter selectors (model, effort, etc.) belong
                // to the previous backend's capability surface; the
                // new backend will publish its own snapshot.
                self.config_options = Vec::new();
                self.config_option_switch_failed = None;
                self.last_agent_switch = Some(AgentSwitchInfo {
                    from,
                    to,
                    reason,
                    switched_at: Utc::now(),
                });
            }
            Event::BackgroundAgentLaunched {
                agent_id,
                tool_call_id,
                description,
                prompt,
                model,
                output_file: _,
                started_at,
            } => {
                let record = BackgroundAgentRecord {
                    agent_id: agent_id.clone(),
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
                };
                // Idempotent on replay / duplicate launch: replace any
                // existing record for the same agent id.
                if let Some(existing) = self
                    .background_agents
                    .iter_mut()
                    .find(|a| a.agent_id == agent_id)
                {
                    *existing = record;
                } else {
                    self.background_agents.push(record);
                }
            }
            Event::BackgroundAgentProgress {
                agent_id,
                status,
                tool_count,
                tools,
                last_tool,
                last_text,
                ..
            } => {
                if let Some(a) = self
                    .background_agents
                    .iter_mut()
                    .find(|a| a.agent_id == agent_id)
                {
                    // A terminal record never reopens to running.
                    if !matches!(
                        a.status,
                        BackgroundAgentStatus::Completed
                            | BackgroundAgentStatus::Detached
                            | BackgroundAgentStatus::Error
                    ) {
                        a.status = status;
                        a.tool_count = tool_count;
                        if !tools.is_empty() {
                            a.tools = tools;
                        }
                        if last_tool.is_some() {
                            a.last_tool = last_tool;
                        }
                        if last_text.is_some() {
                            a.last_text = last_text;
                        }
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
                if let Some(a) = self
                    .background_agents
                    .iter_mut()
                    .find(|a| a.agent_id == agent_id)
                {
                    a.status = status;
                    a.ended_at = Some(ended_at);
                    if !tools.is_empty() {
                        a.tools = tools;
                    }
                    if result.is_some() {
                        a.result = result;
                    }
                    if warning.is_some() {
                        a.warning = warning;
                    }
                }
            }
        }
        self.last_seq = self.last_seq.saturating_add(1);
        self.updated_at = Utc::now();
        Ok(self.last_seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_state() -> AcpState {
        AcpState::new(
            AcpSessionId("s-1".into()),
            AgentName("aoe-agent".into()),
            Some("claude-opus-4-7".into()),
        )
    }

    #[test]
    fn background_agent_lifecycle_builds_record() {
        let mut s = fresh_state();
        s.apply_event(Event::BackgroundAgentLaunched {
            agent_id: "a1".into(),
            tool_call_id: "tc1".into(),
            description: "map backend".into(),
            prompt: "do the thing".into(),
            model: "claude-opus-4-8".into(),
            output_file: "/tmp/a1.output".into(),
            started_at: Utc::now(),
        })
        .unwrap();
        assert_eq!(s.background_agents.len(), 1);
        assert_eq!(
            s.background_agents[0].status,
            BackgroundAgentStatus::Running
        );
        assert_eq!(s.background_agents[0].tool_call_id, "tc1");

        s.apply_event(Event::BackgroundAgentProgress {
            agent_id: "a1".into(),
            status: BackgroundAgentStatus::Running,
            tool_count: 3,
            tools: vec![BackgroundAgentTool {
                name: "Read".into(),
                title: Some("x.rs".into()),
                ok: Some(true),
            }],
            last_tool: Some("Read".into()),
            last_text: Some("scanning".into()),
            at: Utc::now(),
        })
        .unwrap();
        assert_eq!(s.background_agents[0].tool_count, 3);
        assert_eq!(s.background_agents[0].last_tool.as_deref(), Some("Read"));
        assert_eq!(s.background_agents[0].tools.len(), 1);
        assert_eq!(s.background_agents[0].tools[0].name, "Read");

        s.apply_event(Event::BackgroundAgentCompleted {
            agent_id: "a1".into(),
            status: BackgroundAgentStatus::Completed,
            tools: vec![],
            result: Some("done".into()),
            warning: None,
            ended_at: Utc::now(),
        })
        .unwrap();
        assert_eq!(
            s.background_agents[0].status,
            BackgroundAgentStatus::Completed
        );
        assert_eq!(s.background_agents[0].result.as_deref(), Some("done"));

        // A late progress event must not reopen a completed record.
        s.apply_event(Event::BackgroundAgentProgress {
            agent_id: "a1".into(),
            status: BackgroundAgentStatus::Running,
            tool_count: 9,
            tools: vec![],
            last_tool: None,
            last_text: None,
            at: Utc::now(),
        })
        .unwrap();
        assert_eq!(
            s.background_agents[0].status,
            BackgroundAgentStatus::Completed
        );
        assert_eq!(s.background_agents[0].tool_count, 3, "must not overwrite");
    }

    #[test]
    fn apply_event_bumps_seq_and_timestamp() {
        let mut s = fresh_state();
        let before = s.updated_at;
        let seq = s.apply_event(Event::ThinkingStarted).expect("apply ok");
        assert_eq!(seq, 1);
        assert!(s.thinking.is_some());
        assert!(s.updated_at >= before);
    }

    #[test]
    fn mode_switch_failed_bumps_seq_without_mutating_mode() {
        let mut s = fresh_state();
        let before_mode = s.mode;
        let seq = s
            .apply_event(Event::ModeSwitchFailed {
                mode_id: "bypassPermissions".into(),
                reason: "Mode bypassPermissions is not available.".into(),
            })
            .expect("apply ok");
        assert_eq!(seq, 1);
        assert_eq!(s.mode, before_mode);
    }

    #[test]
    fn approval_resolved_with_unknown_nonce_errors() {
        let mut s = fresh_state();
        let result = s.apply_event(Event::ApprovalResolved {
            nonce: Nonce::new(),
            decision: ApprovalDecision::Allow,
        });
        assert!(matches!(result, Err(StateError::UnknownApprovalNonce(_))));
    }

    #[test]
    fn recent_diffs_bounded() {
        let mut s = fresh_state();
        for i in 0..(AcpState::MAX_RECENT_DIFFS + 5) {
            s.apply_event(Event::DiffEmitted {
                diff: DiffPreview {
                    path: format!("/tmp/file{i}.txt"),
                    old_text: None,
                    new_text: Some("hi".into()),
                    created_at: Utc::now(),
                },
            })
            .unwrap();
        }
        assert_eq!(s.recent_diffs.len(), AcpState::MAX_RECENT_DIFFS);
        // Oldest entries dropped first.
        assert!(s.recent_diffs[0].path.contains("file5"));
    }

    #[test]
    fn tool_call_lifecycle() {
        let mut s = fresh_state();
        let tc = ToolCall {
            id: "tc-1".into(),
            name: "Read".into(),
            kind: "read".into(),
            args_preview: "{\"path\":\"x\"}".into(),
            started_at: Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        };
        s.apply_event(Event::ToolCallStarted {
            tool_call: tc.clone(),
        })
        .unwrap();
        assert!(s.in_flight_tool.is_some());
        s.apply_event(Event::ToolCallCompleted {
            tool_call_id: "tc-1".into(),
            is_error: false,
            content: String::new(),
            output: Vec::new(),
            completed_at: Utc::now(),
            async_subagent: false,
        })
        .unwrap();
        assert!(s.in_flight_tool.is_none());
    }

    #[test]
    fn tool_call_updated_replaces_diffs_on_some_and_preserves_on_none() {
        let mut s = fresh_state();
        s.apply_event(Event::ToolCallStarted {
            tool_call: ToolCall {
                id: "tc-1".into(),
                name: "Edit".into(),
                kind: "edit".into(),
                args_preview: "{}".into(),
                started_at: Utc::now(),
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: Vec::new(),
            },
        })
        .unwrap();
        // A later update carrying diff content populates the card.
        s.apply_event(Event::ToolCallUpdated {
            tool_call_id: "tc-1".into(),
            title: None,
            args_preview: None,
            started_at: None,
            diffs: Some(vec![DiffPreview {
                path: "src/foo.rs".into(),
                old_text: Some("old".into()),
                new_text: Some("new".into()),
                created_at: Utc::now(),
            }]),
        })
        .unwrap();
        assert_eq!(s.in_flight_tool.as_ref().unwrap().diffs.len(), 1);
        assert_eq!(
            s.in_flight_tool.as_ref().unwrap().diffs[0].path,
            "src/foo.rs"
        );
        // A subsequent text-only update (diffs None) must not erase them.
        s.apply_event(Event::ToolCallUpdated {
            tool_call_id: "tc-1".into(),
            title: Some("Edit src/foo.rs".into()),
            args_preview: None,
            started_at: None,
            diffs: None,
        })
        .unwrap();
        assert_eq!(
            s.in_flight_tool.as_ref().unwrap().diffs.len(),
            1,
            "text-only update must preserve existing diffs"
        );
    }

    #[test]
    fn available_commands_updated_replaces_previous_list() {
        let mut s = fresh_state();
        assert!(s.available_commands.is_empty());
        s.apply_event(Event::AvailableCommandsUpdated {
            commands: vec![AvailableCommand {
                name: "help".into(),
                description: "Show help".into(),
                accepts_input: false,
            }],
        })
        .unwrap();
        assert_eq!(s.available_commands.len(), 1);
        s.apply_event(Event::AvailableCommandsUpdated {
            commands: vec![
                AvailableCommand {
                    name: "review".into(),
                    description: "Review PR".into(),
                    accepts_input: true,
                },
                AvailableCommand {
                    name: "clear".into(),
                    description: "Clear context".into(),
                    accepts_input: false,
                },
            ],
        })
        .unwrap();
        assert_eq!(s.available_commands.len(), 2);
        assert_eq!(s.available_commands[0].name, "review");
        assert!(s.available_commands[0].accepts_input);
    }

    fn sample_config_options() -> Vec<ConfigOptionDescriptor> {
        vec![
            ConfigOptionDescriptor {
                id: "model".into(),
                name: "Model".into(),
                description: None,
                category: ConfigOptionCategory::Model,
                current_value: "claude-opus-4-7".into(),
                options: vec![
                    ConfigOptionChoice {
                        value: "claude-opus-4-7".into(),
                        name: "Claude Opus 4.7".into(),
                        description: None,
                    },
                    ConfigOptionChoice {
                        value: "claude-sonnet-4-6".into(),
                        name: "Claude Sonnet 4.6".into(),
                        description: None,
                    },
                ],
            },
            ConfigOptionDescriptor {
                id: "effort".into(),
                name: "Reasoning Effort".into(),
                description: None,
                category: ConfigOptionCategory::ThoughtLevel,
                current_value: "default".into(),
                options: vec![
                    ConfigOptionChoice {
                        value: "default".into(),
                        name: "Default".into(),
                        description: None,
                    },
                    ConfigOptionChoice {
                        value: "high".into(),
                        name: "High".into(),
                        description: None,
                    },
                ],
            },
        ]
    }

    #[test]
    fn config_option_category_unknown_value_round_trips_as_other() {
        // Variant-level `#[serde(untagged)]` on `Other(String)` is the
        // canonical pattern (matches upstream `SessionConfigOptionCategory`
        // in agent-client-protocol-schema 0.12). Known snake_case values
        // map to their unit variants; unknown ones fall through into
        // `Other(String)`. Lock both behaviors so a future refactor
        // doesn't silently break forward-compat with new adapter
        // categories.
        let model: ConfigOptionCategory = serde_json::from_str("\"model\"").unwrap();
        assert_eq!(model, ConfigOptionCategory::Model);
        let thought: ConfigOptionCategory = serde_json::from_str("\"thought_level\"").unwrap();
        assert_eq!(thought, ConfigOptionCategory::ThoughtLevel);
        let unknown: ConfigOptionCategory = serde_json::from_str("\"future_category\"").unwrap();
        assert_eq!(
            unknown,
            ConfigOptionCategory::Other("future_category".to_string())
        );
        // Serializing the Other variant preserves the underlying
        // string so the broadcast frame stays stable for clients
        // that don't yet recognize the new category.
        let back = serde_json::to_string(&ConfigOptionCategory::Other("x".into())).unwrap();
        assert_eq!(back, "\"x\"");
    }

    #[test]
    fn config_options_updated_replaces_previous_list() {
        let mut s = fresh_state();
        assert!(s.config_options.is_empty());
        s.apply_event(Event::ConfigOptionsUpdated {
            options: sample_config_options(),
        })
        .unwrap();
        assert_eq!(s.config_options.len(), 2);
        s.apply_event(Event::ConfigOptionsUpdated {
            options: vec![ConfigOptionDescriptor {
                id: "model".into(),
                name: "Model".into(),
                description: None,
                category: ConfigOptionCategory::Model,
                current_value: "claude-sonnet-4-6".into(),
                options: Vec::new(),
            }],
        })
        .unwrap();
        assert_eq!(s.config_options.len(), 1);
        assert_eq!(s.config_options[0].current_value, "claude-sonnet-4-6");
    }

    #[test]
    fn config_option_switch_failed_records_notice_without_mutating_options() {
        let mut s = fresh_state();
        s.apply_event(Event::ConfigOptionsUpdated {
            options: sample_config_options(),
        })
        .unwrap();
        let before = s.config_options.clone();
        s.apply_event(Event::ConfigOptionSwitchFailed {
            config_id: "model".into(),
            value: "claude-sonnet-4-6".into(),
            reason: "rate limited".into(),
        })
        .unwrap();
        let notice = s
            .config_option_switch_failed
            .as_ref()
            .expect("notice populated");
        assert_eq!(notice.config_id, "model");
        assert_eq!(notice.value, "claude-sonnet-4-6");
        assert_eq!(notice.reason, "rate limited");
        assert_eq!(s.config_options, before);
    }

    #[test]
    fn config_options_updated_clears_matching_failure_notice() {
        let mut s = fresh_state();
        s.apply_event(Event::ConfigOptionsUpdated {
            options: sample_config_options(),
        })
        .unwrap();
        s.apply_event(Event::ConfigOptionSwitchFailed {
            config_id: "model".into(),
            value: "claude-sonnet-4-6".into(),
            reason: "transient".into(),
        })
        .unwrap();
        let mut next = sample_config_options();
        next[0].current_value = "claude-sonnet-4-6".into();
        s.apply_event(Event::ConfigOptionsUpdated { options: next })
            .unwrap();
        assert!(s.config_option_switch_failed.is_none());
    }

    #[test]
    fn config_options_updated_preserves_non_matching_failure_notice() {
        let mut s = fresh_state();
        s.apply_event(Event::ConfigOptionsUpdated {
            options: sample_config_options(),
        })
        .unwrap();
        s.apply_event(Event::ConfigOptionSwitchFailed {
            config_id: "model".into(),
            value: "claude-sonnet-4-6".into(),
            reason: "transient".into(),
        })
        .unwrap();
        // Snapshot still shows opus as current; the failure notice for
        // a sonnet switch attempt must survive.
        s.apply_event(Event::ConfigOptionsUpdated {
            options: sample_config_options(),
        })
        .unwrap();
        assert!(s.config_option_switch_failed.is_some());
    }

    #[test]
    fn agent_switched_clears_config_options_and_failure_notice() {
        let mut s = fresh_state();
        s.apply_event(Event::ConfigOptionsUpdated {
            options: sample_config_options(),
        })
        .unwrap();
        s.apply_event(Event::ConfigOptionSwitchFailed {
            config_id: "effort".into(),
            value: "high".into(),
            reason: "unsupported".into(),
        })
        .unwrap();
        s.apply_event(Event::AgentSwitched {
            from: "claude".into(),
            to: "codex".into(),
            reason: "rate_limit".into(),
        })
        .unwrap();
        assert!(s.config_options.is_empty());
        assert!(s.config_option_switch_failed.is_none());
    }

    #[test]
    fn session_cleared_preserves_config_options() {
        let mut s = fresh_state();
        s.apply_event(Event::ConfigOptionsUpdated {
            options: sample_config_options(),
        })
        .unwrap();
        s.apply_event(Event::SessionCleared).unwrap();
        // Adapter capabilities outlive /clear; the model has forgotten
        // the conversation but still advertises the same selectors.
        assert_eq!(s.config_options.len(), 2);
    }

    #[test]
    fn usage_updated_replaces_previous_snapshot() {
        let mut s = fresh_state();
        assert!(s.usage.is_none());
        s.apply_event(Event::UsageUpdated {
            usage: SessionUsage {
                used: 1_000,
                size: 200_000,
                cost: None,
            },
        })
        .unwrap();
        assert_eq!(s.usage.as_ref().map(|u| u.used), Some(1_000));
        s.apply_event(Event::UsageUpdated {
            usage: SessionUsage {
                used: 5_000,
                size: 200_000,
                cost: Some(UsageCost {
                    amount: 0.12,
                    currency: "USD".into(),
                }),
            },
        })
        .unwrap();
        let u = s.usage.as_ref().unwrap();
        assert_eq!(u.used, 5_000);
        assert_eq!(u.cost.as_ref().unwrap().currency, "USD");
    }

    #[test]
    fn rate_limit_auto_resumed_clears_rate_limit_snapshot() {
        let mut s = fresh_state();
        s.apply_event(Event::RateLimit {
            info: RateLimitInfo {
                status: "usage limit reached".into(),
                resets_at: Utc::now(),
                kind: "rate_limit".into(),
            },
        })
        .unwrap();
        assert!(s.rate_limit.is_some(), "RateLimit seeds the park snapshot");
        s.apply_event(Event::RateLimitAutoResumed {
            resets_at: Utc::now(),
        })
        .unwrap();
        assert!(
            s.rate_limit.is_none(),
            "RateLimitAutoResumed must clear the rate-limit snapshot"
        );
    }
}

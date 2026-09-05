// Structured view wire types. Mirror the shapes emitted by the Rust
// `AcpBroadcastFrame` serializer + the `Event` enum in
// `src/acp/state.rs`. These are intentionally permissive: the Rust
// side can add new variants without breaking the UI as long as the
// component renders unknown frames gracefully.

import type { DiffComment } from "../components/diff/comments/types";

export type ApprovalDecision = "Allow" | "AllowAlways" | "Deny" | "Cancelled";

export type SessionMode = "Default" | "Plan" | "AcceptEdits" | "BypassPermissions";

export type PlanStepStatus = "Pending" | "InProgress" | "Done" | "Cancelled";

export interface PlanStep {
  id: string;
  title: string;
  detail?: string | null;
  status: PlanStepStatus;
}

export interface Plan {
  plan_id: string;
  version: number;
  steps: PlanStep[];
}

export interface ToolCall {
  id: string;
  name: string;
  /** The original ACP `ToolCallStarted.name` (the wire tool identity,
   *  e.g. `"task"`). Unlike `name`, this is NEVER overwritten by a later
   *  `ToolCallUpdated.title`, so agent classification can key on the
   *  stable tool identity instead of a mutable display title. Set once at
   *  ingestion; undefined for rows synthesized from an update/completion
   *  that never saw a start frame. See #3070. */
  raw_name?: string;
  /** ACP ToolKind lowercased: read | edit | delete | move | search |
   *  execute | think | fetch | switch_mode | other. Drives the per-tool
   *  renderer in StructuredView. */
  kind: string;
  args_preview: string;
  started_at: string; // ISO-8601 from chrono
  /** When the agent launches a sub-agent (Claude's Task tool), the
   *  adapter rides `_meta.claudeCode.parentToolUseId` along on the
   *  child tool calls. Threaded through here so the structured view can group
   *  sub-tools under their parent Task. Undefined for top-level
   *  calls. See #1041. */
  parent_tool_call_id?: string;
  /** Populated when claude-agent-acp v0.37.0+ routes a session-start
   *  memory recall through the tool channel (upstream #703). The
   *  structured view renders a dedicated MemoryRecallCard instead of treating
   *  it as a generic read. `recall` mode carries the list of file
   *  paths the SDK loaded into the agent's context; `synthesize`
   *  mode carries the synthesised memory text. */
  memory_recall?: MemoryRecall | null;
  /** Structured file diffs the agent attached via ACP
   *  `ToolCallContent::Diff`. Codex routes `apply_patch` edits through
   *  this channel (one entry per touched file) rather than the legacy
   *  `old_string`/`new_string` args, so the edit card reads the path and
   *  +/- preview from here when present and falls back to the args shape
   *  otherwise. See #1721. */
  diffs?: DiffPreview[] | null;
}

export interface MemoryRecall {
  /** "recall" (file list) or "synthesize" (text body). */
  mode: string;
  /** Absolute paths of the memory files loaded into the agent's
   *  context. Empty in synthesize mode. */
  paths?: string[];
  /** Synthesised summary the SDK produced from the loaded memories.
   *  Present in synthesize mode only. */
  synthesized_text?: string | null;
}

export interface DiffPreview {
  path: string;
  old_text?: string | null;
  new_text?: string | null;
  created_at: string;
}

/** One renderable block of a tool call's completion payload, bridged from
 *  an ACP `ToolCallContent` block (mirrors the Rust `ToolOutputBlock`).
 *  Carries the structured media shape so the card renders images / audio /
 *  resources that arrive only at completion instead of collapsing them to
 *  the status word. See #1818. */
export type ToolOutputBlock =
  | { kind: "text"; text: string }
  | {
      kind: "image";
      mime_type: string;
      data?: string | null;
      uri?: string | null;
    }
  | { kind: "audio"; mime_type: string; data?: string | null }
  | {
      kind: "resource_link";
      uri: string;
      name: string;
      mime_type?: string | null;
    }
  | {
      kind: "resource";
      uri: string;
      mime_type?: string | null;
      text?: string | null;
      /** Base64 bytes for a binary (blob) resource, offered as a download
       *  when present. Absent for text resources or oversized blobs. */
      data?: string | null;
    };

export interface RateLimitInfo {
  status: string;
  /** When the quota window clears, or null when the agent never reported
   *  one. Only a reset the agent attributed to the window it rejected gets
   *  here; the alternative was a `now + 1h` guess rendered as fact (#3152).
   *  With null, surface `status`, which usually names the reset in words. */
  resets_at: string | null;
  kind: string;
}

export interface SessionUsage {
  /** Tokens currently in context. */
  used: number;
  /** Total context window size in tokens. */
  size: number;
  /** Cumulative session cost; undefined if the agent doesn't report it. */
  cost?: { amount: number; currency: string } | null;
}

/** One slash command advertised by the agent (mirrors ACP's
 *  `AvailableCommand`). The composer's `/` picker renders these so
 *  users see plugin/skill/MCP commands the agent actually has loaded
 *  rather than a hard-coded placeholder list. */
export interface AvailableCommand {
  name: string;
  description: string;
  /** True when ACP reported an `Unstructured` input spec; i.e. the
   *  command takes free-form arguments after the name. The composer
   *  inserts a trailing space and leaves the cursor in place when
   *  this is true so the user can keep typing. */
  accepts_input: boolean;
}

/** Semantic category for a session configuration option, mirroring
 *  ACP's `SessionConfigOptionCategory`. The structured view UI uses this to
 *  pick the right widget per category (model dropdown, effort
 *  segmented control). The Rust `Other(String)` arm is
 *  `#[serde(untagged)]`, so an unknown category arrives on the wire as
 *  a bare string, not a `{ Other: string }` object. Modeling it as a
 *  catch-all string keeps the broadcast frame forward-compatible while
 *  preserving autocomplete on the known literals. See #1403, #1562. */
export type ConfigOptionCategory = "mode" | "model" | "thought_level" | (string & {});

/** One choice in a `Select`-kind ConfigOptionDescriptor. */
export interface ConfigOptionChoice {
  value: string;
  name: string;
  description?: string | null;
}

/** Structured view's view of a single ACP `SessionConfigOption`. Each
 *  `ConfigOptionsUpdated` event replaces the prior list in full;
 *  the adapter resends the full snapshot whenever any selector
 *  changes. */
export interface ConfigOptionDescriptor {
  id: string;
  name: string;
  description?: string | null;
  category: ConfigOptionCategory;
  current_value: string;
  options: ConfigOptionChoice[];
}

/** Carried by `ConfigOptionSwitchFailed`. Lives on
 *  `AcpState.configOptionSwitchFailed` so the UI can render a
 *  non-blocking notice when the adapter rejects a `set_config_option`
 *  call. Auto-clears when the next `ConfigOptionsUpdated` snapshot
 *  confirms the originally-requested value. */
export interface ConfigOptionSwitchFailure {
  configId: string;
  value: string;
  reason: string;
  at: string;
}

export interface Approval {
  nonce: string;
  tool_call: ToolCall;
  destructive: boolean;
  requested_at: string;
  resolved?: {
    decision: ApprovalDecision;
    message?: string | null;
    resolved_at: string;
  } | null;
}

/** Mirror of `ElicitationFieldKind` in src/acp/elicitations.rs. */
export type ElicitationFieldKind = "free_text" | "single_select" | "multi_select" | "number" | "integer" | "boolean";

export interface ElicitationOption {
  value: string;
  label: string;
  description?: string | null;
}

/** A pre-fill / submitted value. Mirror of `AnswerValue` (untagged): a
 *  string for free-text / single-select, a list for multi-select, a number
 *  for number / integer, a boolean for boolean. */
export type AnswerValue = string | string[] | number | boolean;

export interface ElicitationQuestion {
  field_key: string;
  title?: string | null;
  description?: string | null;
  required: boolean;
  kind: ElicitationFieldKind;
  options: ElicitationOption[];
  /** Multi-select bounds. */
  min_items?: number | null;
  max_items?: number | null;
  /** String bounds (free_text). */
  min_length?: number | null;
  max_length?: number | null;
  /** Regex the string must match (free_text). */
  pattern?: string | null;
  /** Format annotation (`email` / `uri` / `date` / `date-time` / custom),
   *  a UI hint mapped to an input type. */
  format?: string | null;
  /** Numeric bounds (number / integer). */
  minimum?: number | null;
  maximum?: number | null;
  /** Pre-fill value, shaped to match the field kind. */
  default?: AnswerValue | null;
}

/** Mirror of `Elicitation` in src/acp/elicitations.rs: a normalized,
 *  form-mode elicitation the structured view renders. AskUserQuestion is
 *  the common producer; MCP-server forms flow through the same path. */
export interface Elicitation {
  nonce: string;
  message: string;
  /** Schema-level heading (MCP forms may set one; AskUserQuestion does not). */
  title?: string | null;
  /** Schema-level description rendered under the message. */
  description?: string | null;
  tool_call_id?: string | null;
  questions: ElicitationQuestion[];
  requested_at: string;
  resolved?: {
    outcome: ElicitationOutcome;
    resolved_at: string;
  } | null;
}

export type ElicitationOutcome = "Accepted" | "Declined" | "Cancelled";

/** Resolution payload POSTed to
 *  `/api/sessions/{id}/acp/elicitations/{nonce}`. Mirror of
 *  `ElicitationResolution` (tag = "action"). */
export type ElicitationResolution =
  | { action: "accept"; answers: Record<string, AnswerValue> }
  | { action: "decline" }
  | { action: "cancel" };

/** One answered question rendered for the transcript. Mirror of
 *  `ElicitationAnswer` in src/acp/elicitations.rs. Carried on
 *  `ElicitationResolved` (server-rendered) and rebuilt locally by the
 *  optimistic path. See #2209. */
export interface ElicitationAnswer {
  question: string;
  answer: string;
}

/** Narrow a message-metadata payload to a non-empty answer list, so
 *  UserText can pick the card over the plain-text fallback. */
export function isElicitationAnswersPayload(value: unknown): value is ElicitationAnswer[] {
  return (
    Array.isArray(value) &&
    value.length > 0 &&
    value.every(
      (x) =>
        typeof x === "object" &&
        x !== null &&
        typeof (x as ElicitationAnswer).question === "string" &&
        typeof (x as ElicitationAnswer).answer === "string",
    )
  );
}

/** Separator the adapter wedges between an AskUserQuestion option's label and
 *  its description (`"<label> <sep> <description>"`). Written as an escape so
 *  the em dash never appears literally in source. Mirrors `OPTION_DESC_SEP` in
 *  AskUserQuestionCard and `OPTION_DESC_SEP` in src/acp/elicitations.rs. */
const OPTION_DESC_SEP = " \u2014 ";

/** Map a selected option value to its human label. A generic MCP form sends a
 *  machine token as the value and the display text as the label; AskUserQuestion
 *  sends the label as the value (with `label` possibly carrying a trailing
 *  `"value <sep> description"`), so the bare value is kept there. */
function selectLabel(question: ElicitationQuestion, raw: string): string {
  const opt = question.options.find((o) => o.value === raw);
  if (!opt) return raw;
  return opt.label.startsWith(`${raw}${OPTION_DESC_SEP}`) ? raw : opt.label;
}

/** Render a submitted answer value for display, mapping select values to their
 *  option labels. */
function renderAnswerValue(question: ElicitationQuestion, value: AnswerValue): string {
  if (typeof value === "boolean") return value ? "Yes" : "No";
  if (Array.isArray(value)) return value.map((v) => selectLabel(question, v)).join(", ");
  if (typeof value === "string") return selectLabel(question, value);
  return String(value);
}

/** Build display-ready answer pairs from a form and the submitted answers,
 *  in question order, omitting unanswered questions. Mirrors
 *  `summarize_answers` in src/acp/elicitations.rs so the optimistic local
 *  path renders the same row the server broadcasts. */
export function summarizeAnswers(elicitation: Elicitation, answers: Record<string, AnswerValue>): ElicitationAnswer[] {
  const out: ElicitationAnswer[] = [];
  for (const question of elicitation.questions) {
    const value = answers[question.field_key];
    if (value === undefined) continue;
    out.push({
      question: question.title || question.field_key,
      answer: renderAnswerValue(question, value),
    });
  }
  return out;
}

/** Mirror of `StartupErrorDetail` in src/acp/state.rs. Serde's
 *  default for `#[serde(tag = "kind", ...)]` is internal tagging keyed
 *  on `kind`. Carries the structured remediation data the
 *  `StartupErrorScreen` renders. */
export type IncompatibleAgentDetail =
  | {
      kind: "incompatible_agent_version";
      package_name: string;
      installed: string;
      required: string;
      install_command: string;
      /** True when the daemon can `npm install -g` this agent itself, so
       *  the web can offer "Update & restart" (gated on the
       *  `acp.allow_agent_install` setting). See #2109. */
      auto_install: boolean;
    }
  | {
      kind: "missing_agent_info";
      expected_package: string;
      install_command: string;
      auto_install: boolean;
    }
  | {
      kind: "mismatched_agent_name";
      expected: string;
      received: string;
      install_command: string;
      auto_install: boolean;
    }
  | {
      kind: "unparseable_agent_version";
      package_name: string;
      raw_version: string;
      required: string;
      install_command: string;
      auto_install: boolean;
    }
  | {
      kind: "unsupported_protocol_version";
      expected: string;
      received: string;
    };

// One variant per Event::* in src/acp/state.rs. All variants carry
// a discriminant key matching the serde representation: serde defaults
// to externally-tagged JSON for an enum, e.g.
// { "ApprovalRequested": { "approval": ... } }.
export type AcpEvent =
  | { PlanUpdated: { plan: Plan } }
  | {
      TodoListUpdated: {
        todos: Array<{ id: string; text: string; completed: boolean }>;
      };
    }
  | { ToolCallStarted: { tool_call: ToolCall } }
  | {
      ToolCallCompleted: {
        tool_call_id: string;
        is_error: boolean;
        /** Final textual content extracted from
         *  ACP `ToolCallUpdate.fields.content`. Empty when the agent
         *  emitted no content blocks on completion. */
        content: string;
        /** Structured completion payload (images / audio / resources +
         *  text) bridged from the ACP content blocks. Empty/absent for
         *  text-only completions, which render from `content`. See #1818. */
        output?: ToolOutputBlock[];
        /** Server-side ISO-8601 wall clock at which the completion
         *  was minted. Used to stamp the activity row's `at` so the
         *  duration label survives page reload; without it, the
         *  reducer would assign `new Date()` at replay time and the
         *  measured duration would count from "now". Optional for
         *  backward compatibility with events persisted before this
         *  field landed. */
        completed_at?: string;
        /** True when this completion is the synchronous launch of an
         *  async sub-agent (Claude `Task` with isAsync): the call
         *  completes immediately while the real work runs off-protocol
         *  and never reports back on this stream. Renderers draw a
         *  neutral "runs in background" sub-agent card and drop the
         *  marker body (it carries an internal agent id). Absent for
         *  events persisted before this field landed. */
        async_subagent?: boolean;
      };
    }
  | {
      /** Streaming output for a still-running tool call. Carries the
       *  latest full content snapshot (per ACP, content is a
       *  replacement, not append). The reducer buffers it keyed by
       *  tool_call_id and uses it on completion if the final
       *  ToolCallCompleted carries no content of its own. */
      ToolCallContent: { tool_call_id: string; content: string };
    }
  | {
      /** Late-arriving inputs/title for an already-started tool call.
       *  Claude's claude-agent-acp emits the initial tool_call with an
       *  empty `raw_input` and only fills in the actual command in a
       *  follow-up ToolCallUpdate. Without this, bash cards display
       *  `$ Terminal` (the title) rather than the command. */
      ToolCallUpdated: {
        tool_call_id: string;
        title: string | null;
        args_preview: string | null;
        /** Re-stamped start time when the agent reports the tool's
         *  status transitioned to InProgress. See acp_client/update_events.rs;
         *  reused so the duration label measures real tool runtime
         *  rather than adapter scheduling time. Null for non-status
         *  updates. */
        started_at?: string | null;
        /** Diffs carried on a late `ToolCallUpdate.fields.content` frame
         *  (Codex emits apply_patch diffs on the in-progress and
         *  completion updates). A non-empty list REPLACES the card's
         *  diffs; null/absent leaves earlier diffs untouched. See #1721. */
        diffs?: DiffPreview[] | null;
      };
    }
  | { ApprovalRequested: { approval: Approval } }
  | { ApprovalResolved: { nonce: string; decision: ApprovalDecision } }
  | { ElicitationRequested: { elicitation: Elicitation } }
  | {
      ElicitationResolved: {
        nonce: string;
        outcome: ElicitationOutcome;
        answers?: ElicitationAnswer[];
      };
    }
  | "SessionCleared"
  | "ConversationCompactionStarted"
  | "ConversationCompacted"
  | { DiffEmitted: { diff: DiffPreview } }
  | "ThinkingStarted"
  | "ThinkingEnded"
  | { RateLimit: { info: RateLimitInfo } }
  | { RateLimitAutoResumed: { resets_at: string; manual?: boolean } }
  | { UsageUpdated: { usage: SessionUsage } }
  | { ModeChanged: { mode: SessionMode } }
  | {
      ModesAvailable: {
        current_mode_id: string;
        modes: Array<{ id: string; name: string; description?: string | null }>;
      };
    }
  | { CurrentModeChanged: { current_mode_id: string } }
  | { ModeSwitchFailed: { mode_id: string; reason: string } }
  | { AvailableCommandsUpdated: { commands: AvailableCommand[] } }
  | { ConfigOptionsUpdated: { options: ConfigOptionDescriptor[] } }
  | {
      ConfigOptionSwitchFailed: {
        config_id: string;
        value: string;
        reason: string;
      };
    }
  | { RawAgentUpdate: { payload: unknown } }
  | {
      BackgroundAgentLaunched: {
        agent_id: string;
        tool_call_id: string;
        description: string;
        prompt: string;
        model: string;
        started_at: string;
      };
    }
  | {
      BackgroundAgentProgress: {
        agent_id: string;
        status: BackgroundAgentStatus;
        tool_count: number;
        tools?: BackgroundAgentTool[];
        last_tool?: string | null;
        last_text?: string | null;
        at: string;
      };
    }
  | {
      BackgroundAgentCompleted: {
        agent_id: string;
        status: BackgroundAgentStatus;
        tools?: BackgroundAgentTool[];
        result?: string | null;
        warning?: string | null;
        ended_at: string;
      };
    }
  | { AgentMessageChunk: { text: string } }
  | { CancelRequested: { escalates_at: string } }
  | { Stopped: { reason: string } }
  | { AgentStartupError: { message: string } }
  | { PromptRuntimeError: { message: string } }
  | { IncompatibleAgent: { detail: IncompatibleAgentDetail } }
  | {
      UserPromptSent: {
        text: string;
        attachments?: PromptAttachmentRefWire[];
        /** Client-minted stable id echoed back so an optimistic overlay row
         *  reconciles by id. Absent for CLI/TUI/drained-queue prompts and
         *  events persisted before the field landed. See #3173 / Tier 4. */
        prompt_id?: string | null;
      };
    }
  | {
      UserDiffCommentsPrompt: {
        intro: string;
        outro: string;
        isMultiRepo: boolean;
        comments: DiffComment[];
        assembledMarkdown: string;
      };
    }
  | {
      PromptCapabilities: {
        image: boolean;
        audio: boolean;
        embedded_context: boolean;
        /** Absent on events persisted before #2805; the Rust side
         *  defaults it to false, so treat a missing value the same. */
        steering?: boolean;
      };
    }
  | { AcpSessionAssigned: { acp_session_id: string } }
  | { SessionContextReset: { reason: string } }
  | { WakeupScheduled: { at: string; reason: string | null } }
  | { MonitorArmed: { description: string | null } }
  | { PromptRejected: { reason: string; text: string } }
  | { AgentSwitched: { from: string; to: string; reason: string } }
  | { ConversationSummary: { text: string; summarized_until_seq: number } };

/** Metadata-only attachment ref as it rides on a `UserPromptSent`
 *  event from the server (mirrors Rust `PromptAttachmentRef`). The
 *  bytes are fetched lazily from the replay GET endpoint. See #1000. */
export interface PromptAttachmentRefWire {
  id: string;
  kind: PromptAttachmentKind;
  mime_type: string;
  name?: string;
  size: number;
}

export type PromptAttachmentKind = "image" | "audio" | "resource";

/** What the agent will accept on a prompt, from the ACP `initialize`
 *  handshake. Drives the composer's attachment button gating. */
export interface PromptCapabilities {
  image: boolean;
  audio: boolean;
  embeddedContext: boolean;
  /** Whether the agent accepts `_session/steering`, so a prompt sent
   *  mid-turn is injected into the running turn instead of parked in
   *  the composer queue. Re-emitted on every connect, including as
   *  false, so it cannot go stale after a respawn onto an adapter
   *  without the capability. See #2805. */
  steering: boolean;
}

/** One attachment as the composer hands it to `sendPrompt`: the raw
 *  base64 bytes plus metadata. The hook turns this into both the POST
 *  upload body and the optimistic preview row. See #1000 / #965. */
export interface PromptAttachmentInput {
  kind: PromptAttachmentKind;
  mimeType: string;
  name?: string;
  /** Standard base64, no `data:` URL prefix. */
  dataB64: string;
}

/** One attachment as the composer and transcript render it. `url` is
 *  the replay GET endpoint for server-confirmed rows, or a local
 *  object URL for the optimistic echo before the server confirms. */
export interface AcpAttachment {
  id: string;
  kind: PromptAttachmentKind;
  mimeType: string;
  name?: string;
  size: number;
  url: string;
}

export interface AcpFrame {
  session_id: string;
  seq: number;
  event: AcpEvent;
}

/** Fields this client adopts from the daemon's folded `AcpState`. Worker
 *  lifecycle and optimistic UI state remain client-side. */
export interface ReducedState {
  agent: string;
  model: string | null;
  mode: SessionMode;
  current_plan: Plan | null;
  in_flight_tool: ToolCall | null;
  pending_approvals: Approval[];
  pending_elicitations: Elicitation[];
  thinking: { started_at: string } | null;
  rate_limit: RateLimitInfo | null;
  available_commands: AvailableCommand[];
  available_modes: Array<{ id: string; name: string; description?: string | null }>;
  current_mode_id: string | null;
  turn_active: boolean;
  cancelling: boolean;
  compacting: boolean;
}

export interface AcpState {
  agent: string | null;
  model: string | null;
  mode: SessionMode;
  /** Attachment kinds the current agent accepts, from the latest
   *  `PromptCapabilities` event. Null until the handshake reports it;
   *  the composer keeps the attachment button disabled while null. */
  promptCapabilities: PromptCapabilities | null;
  plan: Plan | null;
  inFlightTool: ToolCall | null;
  pendingApprovals: Approval[];
  /** Pending AskUserQuestion elicitations awaiting a user answer. */
  pendingElicitations: Elicitation[];
  /** Nonces of approvals / elicitations answered here, held until the
   *  daemon's own pending list stops carrying them. See
   *  {@link applyReducedState}. */
  locallyResolved: string[];
  thinking: boolean;
  rateLimit: RateLimitInfo | null;
  /** Latest agent-reported context-window usage. Null until the agent
   *  emits its first ACP `UsageUpdate`. */
  sessionUsage: SessionUsage | null;
  /** Cost at the latest context boundary, subtracted from the agent's
   *  session-lifetime total. */
  usageBaseline: { cost: number } | null;
  /** Usage when the compaction reminder was dismissed, or null while armed.
   *  Stored in reducer state so it survives session switches and reloads. */
  compactionReminderDismissed: SessionUsage | null;
  /** Server-folded transcript rows in oldest-first order. */
  activity: ActivityRow[];
  /** Unpersisted optimistic rows, removed when the server row with the same id
   *  arrives. */
  optimisticRows: ActivityRow[];
  /** Last seen seq, for reconnect requests. Frames whose `seq` is
   *  not strictly greater than this are dropped by the reducer so
   *  reconnect-replay can deliver the same frames again without
   *  double-applying them to state. */
  lastSeq: number;
  /** Lowest seq whose rows are currently in `activity`, i.e. the
   *  recent-first load watermark. 0 means nothing loaded yet (or the
   *  whole history is loaded down to the start). The client pages older
   *  history by requesting `?before=<oldestSeq>`; the reducer's `prepend`
   *  action lowers it. See #2236. */
  oldestSeq: number;
  /** True if the most recent broadcast told us we lagged. Cleared
   *  the next time the client successfully resyncs via the snapshot
   *  endpoint. */
  lagged: boolean;
  /** Latest agent startup failure message, if any. Cleared when a new
   *  prompt is sent or the worker successfully connects. */
  startupError: string | null;
  /** Structured detail from the per-adapter compatibility check (see
   *  `src/acp/agent_compat.rs`). When set, the structured view UI replaces
   *  its normal session view with a full-region `StartupErrorScreen`
   *  that renders the exact remediation command. Distinct from
   *  `startupError` (string) which legacy callers still populate for
   *  free-form handshake failures; `incompatibleAgent` carries
   *  installed/required versions + install command in structured form.
   *  Cleared on a fresh `AcpSessionAssigned` so a respawned worker
   *  that satisfies the policy unblocks the UI. */
  incompatibleAgent: IncompatibleAgentDetail | null;
  /** Latest interaction error (failed sendPrompt / resolveApproval /
   *  cancel POST). Surfaces as a dismissible banner so users don't
   *  silently lose actions to a network blip. Cleared on the next
   *  successful interaction. */
  lastError: string | null;
  /** True between sending a user prompt and receiving the
   *  `Stopped { reason: "prompt_complete" }` event. Drives the global
   *  "working" spinner so the UI feels alive even when the agent
   *  isn't streaming text or running a tool yet.
   *
   *  Derived from `serverTurnActive || inflightPromptIds.length > 0`;
   *  never written directly. Keeping it on the state shape (instead of
   *  exporting a selector) lets all the existing `state.turnActive`
   *  reads stay unchanged. See {@link deriveTurnActive} and #3417. */
  turnActive: boolean;
  /** The daemon's `AcpState.turn_active`, adopted verbatim from the
   *  `reduced_state` frame. Authoritative for the steady state: the
   *  daemon is the only party that knows whether a mid-turn prompt was
   *  steered into the running turn (one terminal `Stopped` for many
   *  prompts) or opened a turn of its own. See #3417 / #2805. */
  serverTurnActive: boolean;
  /** Client-minted prompt ids POSTed but not yet acknowledged, either by
   *  the server's `UserPromptSent` echo or by the POST settling with a
   *  failure. The thin optimistic overlay that covers the POST-to-echo
   *  gap so the composer flips to "working" instantly; correlated by id
   *  so one prompt settling cannot retire another's turn. Request-local,
   *  never persisted. */
  inflightPromptIds: string[];
  /** Monotonic count of user prompts dispatched, never decremented. Bumped
   *  by the optimistic `user_prompt` action and by any `UserPromptSent` with
   *  no matching outstanding optimistic id (a replay, another device, a
   *  drained queue entry), so an echo of this client's own prompt counts once.
   *
   *  Two readers: `useCancelEscalation` tokenises "already pressed Stop for
   *  this prompt" as `(sessionId, promptSeq)` so the next prompt's first Stop
   *  is graceful again (#2237), and the `SessionContextReset` arm treats zero
   *  as "this session never had a prompt to lose" and suppresses the re-prime
   *  offer. Deliberately not turn truth: see {@link deriveTurnActive}. */
  promptSeq: number;
  /** Real ACP-advertised modes from the agent's NewSessionResponse,
   *  plus the agent's currently-active mode id. Empty until the
   *  agent reports them; the picker falls back to the hard-coded
   *  four-mode taxonomy in that case. */
  availableModes: Array<{
    id: string;
    name: string;
    description?: string | null;
  }>;
  currentModeId: string | null;
  /** Slash commands the agent advertised in its most recent
   *  `AvailableCommandsUpdate`. Empty until the agent emits one; the
   *  composer's `/` picker reads from here. */
  availableCommands: AvailableCommand[];
  /** Latest acp-side `session/set_mode` rejection from the adapter.
   *  Populated by the `ModeSwitchFailed` event so the UI can render a
   *  non-blocking notice ("Yolo / bypassPermissions requested but the
   *  adapter declined; session is in default mode"). Most common cause:
   *  claude-agent-acp gates bypassPermissions on the `ALLOW_BYPASS` env
   *  var. Cleared by the user dismissing the notice or by a successful
   *  `CurrentModeChanged`. See #1233. */
  modeSwitchFailed: { modeId: string; reason: string; at: string } | null;
  /** Set true when the daemon publishes `Stopped { reason: "user_stopped" }`,
   *  meaning `aoe acp stop|kill` (or an equivalent external
   *  teardown) terminated the runner. The composer disables itself and
   *  shows a reconnect banner; cleared on the next UserPromptSent or
   *  AcpSessionAssigned (a fresh worker is online). */
  workerStopped: boolean;
  /** Set true when the daemon publishes `Stopped { reason: "restart_pending" }`,
   *  meaning `aoe acp restart` ran and the reconciler will respawn
   *  the worker on its next 2s tick with the cached `acp_session_id`
   *  (transcript continuity). The composer disables itself and a
   *  transient "Restarting…" banner appears without a reconnect button;
   *  cleared on AcpSessionAssigned or UserPromptSent. */
  workerRestarting: boolean;
  /** Set true when the daemon publishes `Stopped { reason: "idle_auto_stop" }`,
   *  meaning the reconciler reaped the worker for inactivity
   *  (`acp.auto_stop_idle_secs`) and marked the session dormant. Unlike
   *  `workerStopped`, this is recoverable without any explicit reconnect:
   *  the next prompt POST wakes it (the server's `touch_on_prompt_and_wake_if_sunk`
   *  clears dormancy, the reconciler respawns, and `send_prompt`'s
   *  `wait_for_worker` holds the request until the fresh worker is ready).
   *  `sendPrompt` and the drain effect read this so a dormant worker does
   *  NOT park prompts in the local queue forever; instead the POST itself
   *  is the wake path. Cleared on AcpSessionAssigned or UserPromptSent. */
  workerIdleStopped: boolean;
  /** Set true when the daemon publishes
   *  `Stopped { reason: "rate_limit_exhausted_retries" }`: auto-resume gave
   *  up re-delivering the interrupted prompt after its cap and parked the
   *  session (#3688). The banner says so; RESUME NOW and a fresh prompt
   *  both clear it. */
  rateLimitRetriesExhausted: boolean;
  /** Follow-up prompts the user typed and submitted while a turn was
   *  already running. The composer enqueues them client-side instead
   *  of racing the agent (claude-agent-acp serialises session/prompt
   *  internally, but client-side queueing gives us a visible "queued"
   *  badge and lets the user edit / drop entries before they fire).
   *  On `Stopped` (when the worker is healthy) the head is popped and
   *  dispatched via the regular sendPrompt path. See #1031. */
  queuedPrompts: QueuedPrompt[];
  /** ISO-8601 timestamp at which the agent's pending `ScheduleWakeup`
   *  fires (i.e. when the next /loop turn is expected to start).
   *  Cleared by `UserPromptSent` since /loop self-fires a prompt on
   *  wake. See #1091. */
  nextWakeupAt: string | null;
  /** Reason the agent provided when scheduling the wakeup. Shown in
   *  the structured view banner next to the countdown. */
  nextWakeupReason: string | null;
  /** True when the agent has an armed `Monitor` (a background watch).
   *  Unlike a scheduled wakeup it has no fire time, so the UI shows a
   *  static "monitoring" badge, not a countdown. A monitor firing
   *  re-invokes the agent with activity but never a `UserPromptSent`, so
   *  this persists across the wait and clears when the user takes over
   *  (`UserPromptSent`) or the monitor fires and that turn ends: a tool
   *  call started after the arm (`monitorWorkSeen`) followed by a
   *  `Stopped`. See #2325. */
  monitorArmed: boolean;
  /** True once a tool call has started after the latest `MonitorArmed`,
   *  i.e. the monitor fired and the agent acted on it. Gates the badge
   *  clear on the next `Stopped` so the arming turn ending while the
   *  monitor is still pending does not clear it. Reset by `MonitorArmed`
   *  and `UserPromptSent`. See #2325. */
  monitorWorkSeen: boolean;
  /** The `description` the agent gave the `Monitor` tool, shown as the
   *  badge tooltip. Null when none was provided or no monitor is armed. */
  monitorDescription: string | null;
  /** True between a `CancelRequested` event (aoe sent `session/cancel`
   *  and armed the escalation watchdog) and the next `Stopped`. Drives
   *  the "Stopping..." spinner label and reveals the Force-stop
   *  affordance even while a tool is in flight. Cleared on any `Stopped`
   *  and on a fresh `UserPromptSent`. See #1727. */
  cancelling: boolean;
  /** ISO-8601 timestamp at which the cancel-escalation watchdog will
   *  SIGTERM the worker if the agent keeps ignoring the cancel. Lets the
   *  UI show an honest countdown. Null when not cancelling. See #1727. */
  cancelEscalatesAt: string | null;
  /** True between `ConversationCompactionStarted` and the matching
   *  `ConversationCompacted`, or the turn's `Stopped` if the completion
   *  marker never lands. The adapter goes silent for 90 to 170 seconds in
   *  that window, so this keeps the stall watchdog from relabelling the
   *  spinner "Waiting on model" and offering a Force-end-turn button that
   *  would kill the compaction, and parks a follow-up in the queue
   *  instead of steering it into a turn that never answers it.
   *
   *  Deliberately NOT cleared by `applyNewTurnResets`: that runs on every
   *  server-confirmed `UserPromptSent`, so a prompt confirmed mid-window
   *  would drop the phase while compaction is still running. See #3219. */
  compacting: boolean;
  /** Set when the agent emitted `SessionContextReset` after a prior
   *  user prompt: the model's context is empty but the visible
   *  transcript is intact, so the user can opt in to fetching a
   *  primer (last N turns) and pre-filling the composer with it.
   *  Cleared by `UserPromptSent`. See #1004. */
  contextPrimerAvailable: { resetSeq: number; reason: string } | null;
  /** Capped FIFO of prompts the daemon rejected because another
   *  `session/prompt` was already in flight. The composer renders a
   *  Retry pill per entry; clicking Retry re-dispatches via the
   *  normal sendPrompt path. Cleared on `UserPromptSent` (the user
   *  has either retried or moved on). See #1196. */
  rejectedPrompts: RejectedPrompt[];
  /** Set when the daemon emitted `Stopped { reason: "agent_unresponsive" }`,
   *  meaning the cancel-escalation watchdog fired and the supervisor
   *  is restarting the wedged worker. The composer renders a specific
   *  banner ("Agent stopped responding to cancel, restarting worker")
   *  instead of the generic "Restarting..." overlay. Also pairs with
   *  `workerRestarting = true` so the existing composer-lockdown
   *  styling kicks in; cleared on `AcpSessionAssigned` (the respawned
   *  worker came online) or `UserPromptSent`. See #1196. */
  agentUnresponsive: boolean;
  /** Most recent `AgentSwitched` snapshot. Populated when the user
   *  hands off a rate-limited session to a different ACP backend via
   *  `/acp/switch-agent`. Drives a transcript divider ("Switched
   *  claude -> codex due to rate_limit") and lets the recovery flow
   *  identify the cursor where the handoff happened. Cleared by
   *  `SessionCleared`. See #1282. */
  lastAgentSwitch: {
    from: string;
    to: string;
    reason: string;
    at: string;
  } | null;
  /** Full snapshot of the per-session selectors (model, reasoning
   *  effort, mode, future categories) the adapter advertises through
   *  ACP `SessionUpdate::ConfigOptionUpdate`. Empty when the adapter
   *  emits no config options. Replaced wholesale on each
   *  `ConfigOptionsUpdated` frame; cleared on `AgentSwitched`. See
   *  #1403. */
  configOptions: ConfigOptionDescriptor[];
  /** Non-blocking notice for the most recent
   *  `session/set_config_option` rejection. Auto-clears when the
   *  next snapshot confirms the originally-requested value, or on
   *  `AgentSwitched`. */
  configOptionSwitchFailed: ConfigOptionSwitchFailure | null;
  /** Set when the user clicks a model/effort option and the POST is
   *  in flight; cleared by the next `ConfigOptionsUpdated` snapshot
   *  (which reconciles authoritative state) or by
   *  `ConfigOptionSwitchFailed`. Drives the pending affordance
   *  (opacity dim + disabled re-click) on the just-clicked option;
   *  the picker keeps showing the previously-current value until the
   *  adapter confirms, so the UI never lies about active state. */
  pendingConfigOption: { configId: string; value: string } | null;
  /** Set when the daemon emitted `Stopped { reason: "prompt_orphaned" }`,
   *  meaning the silent-orphan watchdog detected that the adapter
   *  finished streaming the turn but never sent the JSON-RPC
   *  `PromptResponse`. The supervisor is SIGTERMing the runner and
   *  respawning via `session/load` (transcript preserved). Pairs with
   *  `workerRestarting = true` for composer lockdown; banner copy
   *  distinguishes this from `agentUnresponsive` so users can tell
   *  whether the adapter ignored their cancel (`agentUnresponsive`)
   *  or finished without notifying the daemon (`agentOrphaned`).
   *  Cleared on `AcpSessionAssigned` or `UserPromptSent`. See #1240. */
  agentOrphaned: boolean;
  /** Async sub-agents (Claude `Task` with isAsync) launched this session.
   *  The parent stream only carries each launch; the daemon tails each
   *  agent's transcript and emits `BackgroundAgent*` events that build
   *  this list. Drives the Background agents panel and the inline Task
   *  card linkage. Insertion order (oldest first). */
  backgroundAgents: BackgroundAgent[];
}

/** Lifecycle status of an async background sub-agent. Mirrors the Rust
 *  `BackgroundAgentStatus`. `completed` is the only clean-finish state. */
export type BackgroundAgentStatus = "running" | "stalled" | "completed" | "detached" | "error";

/** One tool call a background sub-agent made, for the per-agent tool list.
 *  Mirrors the Rust `BackgroundAgentTool`. */
export interface BackgroundAgentTool {
  name: string;
  /** Short label from the tool input (command / file path / pattern). */
  title?: string | null;
  /** Outcome: undefined while running, true succeeded, false errored. */
  ok?: boolean | null;
}

/** One async background sub-agent, built up from `BackgroundAgent*`
 *  events. Mirrors the Rust `BackgroundAgentRecord`. */
export interface BackgroundAgent {
  agentId: string;
  /** The parent `Task` tool call that launched this agent; links the
   *  inline tool card to this panel entry. */
  toolCallId: string;
  description: string;
  prompt: string;
  model: string;
  status: BackgroundAgentStatus;
  /** ISO-8601 launch time. */
  startedAt: string;
  /** ISO-8601 terminal time, set on completion/stall/error. */
  endedAt: string | null;
  toolCount: number;
  /** Individual tool calls in order, like the main output. */
  tools: BackgroundAgentTool[];
  lastTool: string | null;
  lastText: string | null;
  result: string | null;
  warning: string | null;
}

export interface RejectedPrompt {
  /** Client-stable id derived from the frame seq. Used to key the
   *  pill list and to target a specific entry for retry/dismiss. */
  id: string;
  text: string;
  reason: string;
  /** Server-side wall-clock at rejection time (frame arrival). */
  rejectedAt: string;
}

export interface QueuedPrompt {
  /** Client-minted id; survives edits. Used by the composer strip to
   *  key the list and by the edit / delete actions to target a row. */
  id: string;
  text: string;
  /** ISO-8601 client wall clock at enqueue time. Displayed as a
   *  relative age in the strip. */
  queuedAt: string;
  /** Attachments staged with this queued prompt. The bytes now live
   *  server-side (the pending-attachment store) and are delivered on drain;
   *  a locally-queued row keeps the raw base64 in memory so the strip can
   *  render a thumbnail until the server confirms. `persistState` still drops
   *  any queued row that has them rather than writing megabytes into the
   *  per-origin localStorage quota; a hydrate from the server repopulates the
   *  metadata (id/kind/mime/name, no bytes) after reload. See #1833 / #1000. */
  attachments?: PromptAttachmentInput[];
  /** True for an optimistic row whose server enqueue POST has not been
   *  confirmed yet. A hydrate from the server keeps `pending` rows that are
   *  not (yet) in the server snapshot, so an in-flight enqueue is not dropped
   *  by a hydrate that races the POST. Cleared once the row appears in a
   *  server snapshot. */
  pending?: boolean;
}

export interface ActivityRow {
  id: string;
  kind:
    | "tool_start"
    | "tool_complete"
    | "tool_error"
    | "tool_stopped"
    | "message"
    | "thinking"
    | "user_prompt"
    | "user_diff_comments"
    | "elicitation_answered"
    | "empty_output"
    | "context_reset"
    | "notice"
    | "session_cleared"
    | "compacted"
    | "summary";
  text: string;
  toolCallId?: string;
  /** Full ToolCall payload, present on tool_start rows so the UI can
   *  pick a per-kind renderer without needing to look the call up by
   *  toolCallId. */
  tool?: ToolCall;
  /** Structured payload on `user_diff_comments` rows. The runtime
   *  attaches it to the assistant-ui message metadata so the
   *  transcript renders the rich `DiffCommentsUserCard`; `text` holds
   *  the assembled markdown as the fallback / agent-visible body. */
  diffComments?: {
    intro: string;
    outro: string;
    isMultiRepo: boolean;
    comments: DiffComment[];
  };
  /** Attachments on a `user_prompt` row (images / audio / resources).
   *  Set from the optimistic local preview on send, or from the
   *  server `UserPromptSent` refs on replay. See #1000 / #965. */
  attachments?: AcpAttachment[];
  /** Structured completion payload on `tool_complete` / `tool_error`
   *  rows: media/resource blocks the card renders richly when the agent
   *  ships them only at completion. Absent for text-only completions
   *  (those render from `text`). See #1818. */
  output?: ToolOutputBlock[];
  /** Display-ready answers on an `elicitation_answered` row (the user's
   *  reply to an AskUserQuestion / elicitation form). `text` holds a flat
   *  fallback; the card renders the structured pairs. See #2209. */
  elicitationAnswers?: ElicitationAnswer[];
  /** True on a `tool_complete` row that is the synchronous launch of an
   *  async sub-agent (Claude `Task` with isAsync). The runtime routes it
   *  to a neutral background sub-agent card and drops the marker body. */
  asyncSubagent?: boolean;
  at: string; // ISO-8601
}

/** Wire mirror of the Rust `TranscriptRow` (src/acp/transcript.rs), serde
 *  snake_case. The server folds the event stream into these ordered rows and
 *  ships them via the WS transcript channel and `GET /acp/replay?view=rows`;
 *  the client maps each to an {@link ActivityRow} and renders it instead of
 *  re-reducing frames. The `kind` set matches `ActivityRow["kind"]` minus
 *  `thinking` (which is control-state, never a transcript row). */
export interface TranscriptRow {
  id: string;
  group_id: string;
  kind: Exclude<ActivityRow["kind"], "thinking">;
  at: string;
  text: string;
  tool_call_id?: string;
  tool?: ToolCall;
  output?: ToolOutputBlock[];
  attachments?: PromptAttachmentRefWire[];
  diff_comments?: {
    intro: string;
    outro: string;
    is_multi_repo: boolean;
    comments: DiffComment[];
  };
  elicitation_answers?: ElicitationAnswer[];
  async_subagent?: boolean;
}

/** Wire mirror of the Rust `TranscriptDelta` (externally tagged enum). One
 *  incremental change to the ordered row list, emitted per event on the WS
 *  `transcript_delta` frame. */
export type TranscriptDelta =
  | { Append: TranscriptRow }
  | { Patch: { id: string; row: TranscriptRow } }
  | { Remove: string };

/** Map a server {@link TranscriptRow} to the client {@link ActivityRow} shape
 *  (snake_case -> camelCase) the renderers already consume, so `AcpRuntime` /
 *  `ToolCards` / `Markdown` stay unchanged. `attachments` gain their lazy
 *  replay-GET url from the session id. `raw_name` is not on the wire (the Rust
 *  `ToolCall` has no such field, #3070), so it is best-effort seeded from
 *  `name`. */
/** Whether this client renders a given server row.
 *
 *  The daemon emits a `notice` row for a failed startup, a turn that died
 *  mid-flight, a refused mode switch, or a rate-limit auto-resume, because the
 *  native view shows those inline in the timeline. The web surfaces the same
 *  information as dismissible banners driven by `startupError` / `lastError` /
 *  `modeSwitchFailed`, which it still folds from raw frames, so rendering the
 *  row too would say it twice. */
export function webRendersServerRow(row: TranscriptRow): boolean {
  return row.kind !== "notice";
}

export function transcriptRowToActivity(row: TranscriptRow, sessionId: string): ActivityRow {
  const tool: ToolCall | undefined = row.tool
    ? { ...row.tool, raw_name: row.tool.raw_name ?? row.tool.name }
    : undefined;
  const attachments: AcpAttachment[] | undefined =
    row.attachments && row.attachments.length > 0
      ? row.attachments.map((a) => ({
          id: a.id,
          kind: a.kind,
          mimeType: a.mime_type,
          name: a.name,
          size: a.size,
          url: `/api/sessions/${encodeURIComponent(sessionId)}/acp/attachments/${encodeURIComponent(a.id)}`,
        }))
      : undefined;
  return {
    id: row.id,
    kind: row.kind,
    text: row.text,
    at: row.at,
    ...(row.tool_call_id ? { toolCallId: row.tool_call_id } : {}),
    ...(tool ? { tool } : {}),
    ...(row.output && row.output.length > 0 ? { output: row.output } : {}),
    ...(attachments ? { attachments } : {}),
    ...(row.diff_comments
      ? {
          diffComments: {
            intro: row.diff_comments.intro,
            outro: row.diff_comments.outro,
            isMultiRepo: row.diff_comments.is_multi_repo,
            comments: row.diff_comments.comments,
          },
        }
      : {}),
    ...(row.elicitation_answers && row.elicitation_answers.length > 0
      ? { elicitationAnswers: row.elicitation_answers }
      : {}),
    ...(row.async_subagent ? { asyncSubagent: true } : {}),
  };
}

/** Append / merge a batch of server-folded rows onto the existing activity
 *  tail, reconciling by the server's deterministic row id. A row whose id
 *  already exists replaces it in place (the server is authoritative); two
 *  `tool_start` rows for one id are merged so a sparse synth start folded on a
 *  later replay page (the server's `?view=rows` folds each page in isolation)
 *  cannot clobber a richer start already loaded (the #1713/#2711 seam). New
 *  ids append in order. Idempotent, which absorbs the WS/replay overlap race
 *  (#1100) without a seq gate. */
export function mergeServerRows(existing: ActivityRow[], incoming: ActivityRow[]): ActivityRow[] {
  if (incoming.length === 0) return existing;
  const indexById = new Map<string, number>();
  existing.forEach((r, i) => indexById.set(r.id, i));
  let out = existing;
  const ensureCopy = () => {
    if (out === existing) out = existing.slice();
  };
  for (const row of incoming) {
    const idx = indexById.get(row.id);
    if (idx === undefined) {
      ensureCopy();
      indexById.set(row.id, out.length);
      out.push(row);
      continue;
    }
    ensureCopy();
    const prev = out[idx]!;
    if (prev.kind === "tool_start" && row.kind === "tool_start" && prev.tool && row.tool) {
      const merged = mergeToolStart(prev.tool, row.tool);
      // raw_name is the immutable ACP wire tool identity, but the server's
      // ToolCall has no such field (#3070): the client derives it from the
      // first-seen `name` in `transcriptRowToActivity`, so keep the earliest
      // one across a retitling merge for subagent classification.
      if (prev.tool.raw_name) merged.raw_name = prev.tool.raw_name;
      out[idx] = { ...prev, tool: merged, text: merged.name, at: merged.started_at };
    } else {
      out[idx] = row;
    }
  }
  return out;
}

/** Replace the row with `row.id` by the server's authoritative new row (a
 *  `Patch` transcript delta carries the full row). Appends when the id is not
 *  present, e.g. a Patch that lands before its Append after a reconnect.
 *  Preserves the earliest-seen `raw_name` on a `tool_start` patch, since a
 *  retitling `ToolCallUpdated` overwrites the server row's `name` and the wire
 *  identity would otherwise be lost (#3070). */
export function patchServerRow(existing: ActivityRow[], row: ActivityRow): ActivityRow[] {
  const idx = existing.findIndex((r) => r.id === row.id);
  if (idx === -1) return existing.concat(row);
  const prev = existing[idx]!;
  const next = existing.slice();
  if (prev.tool?.raw_name && row.tool && row.tool.raw_name !== prev.tool.raw_name) {
    next[idx] = { ...row, tool: { ...row.tool, raw_name: prev.tool.raw_name } };
  } else {
    next[idx] = row;
  }
  return next;
}

export function emptyAcpState(): AcpState {
  return {
    agent: null,
    model: null,
    mode: "Default",
    promptCapabilities: null,
    plan: null,
    inFlightTool: null,
    pendingApprovals: [],
    pendingElicitations: [],
    locallyResolved: [],
    thinking: false,
    rateLimit: null,
    rateLimitRetriesExhausted: false,
    sessionUsage: null,
    usageBaseline: null,
    compactionReminderDismissed: null,
    activity: [],
    optimisticRows: [],
    lastSeq: 0,
    oldestSeq: 0,
    lagged: false,
    startupError: null,
    incompatibleAgent: null,
    lastError: null,
    turnActive: false,
    serverTurnActive: false,
    inflightPromptIds: [],
    promptSeq: 0,
    availableModes: [],
    currentModeId: null,
    availableCommands: [],
    workerStopped: false,
    workerRestarting: false,
    workerIdleStopped: false,
    queuedPrompts: [],
    nextWakeupAt: null,
    nextWakeupReason: null,
    monitorArmed: false,
    monitorWorkSeen: false,
    monitorDescription: null,
    cancelling: false,
    cancelEscalatesAt: null,
    compacting: false,
    contextPrimerAvailable: null,
    rejectedPrompts: [],
    agentUnresponsive: false,
    agentOrphaned: false,
    backgroundAgents: [],
    modeSwitchFailed: null,
    lastAgentSwitch: null,
    configOptions: [],
    configOptionSwitchFailed: null,
    pendingConfigOption: null,
  };
}

/** Whether a `UserPromptSent` is a message steered into the turn already
 *  running rather than the start of a new one (#2805).
 *
 *  The daemon injects a mid-turn prompt via `_session/steering` instead of
 *  starting a turn for it, so the same condition the composer used to send
 *  it identifies it on the way back. Such a prompt must NOT run
 *  {@link applyNewTurnResets}: there is no new turn, and the running
 *  turn's single `Stopped` still has to see the output flag and the
 *  pending-cancel state the turn actually accumulated.
 *
 *  Takes the pre-event state, since the arms open the turn before they
 *  reach the reset.
 *
 *  Reads `serverTurnActive`, not the rendered `turnActive`: since #3417 the
 *  latter is also true through the POST-to-echo gap of this client's own
 *  prompt, so an idle session's very first prompt would look steered to its
 *  own echo and skip the resets it needs. Only the daemon's flag means "a
 *  turn was already running".
 */
function isSteeredContinuation(state: AcpState): boolean {
  return state.serverTurnActive && !!state.promptCapabilities?.steering;
}

/** Per-turn state resets shared by every "a new user turn started"
 *  event (a plain `UserPromptSent` and a `UserDiffCommentsPrompt`).
 *  Mutates `next` in place; the caller has already opened the turn. */
function applyNewTurnResets(next: AcpState): void {
  next.startupError = null;
  next.lastError = null;
  // The cancel phase itself is server-owned (a fresh non-steered turn clears
  // it there); only the escalation deadline is ours to drop. See #1727.
  next.cancelEscalatesAt = null;
  // A fresh prompt means the worker is alive again; clear the
  // user_stopped banner without waiting for AcpSessionAssigned.
  next.workerStopped = false;
  next.workerRestarting = false;
  // A prompt also wakes an idle-dormant worker (the POST cleared
  // dormancy server-side); drop the marker so the drain effect stops
  // treating the worker as wakeable-but-down.
  next.workerIdleStopped = false;
  // A fresh prompt also recovers an exhausted-retries park (#3688): the
  // POST wakes the worker and the turn supersedes the dropped continuation.
  next.rateLimitRetriesExhausted = false;
  // The user is moving on. Clear any pending Retry pills and the
  // agent-unresponsive banner; if the rejection was legitimate the
  // new prompt will end up rejected too and a fresh pill will land.
  // See #1196.
  next.rejectedPrompts = [];
  next.agentUnresponsive = false;
  next.agentOrphaned = false;
  // /loop dynamic mode self-fires a prompt on wake, but a user-typed
  // follow-up during the wait is NOT the wake firing; only clear when
  // the scheduled time has already elapsed. The countdown UI continues
  // counting down through a mid-wait user prompt; the next
  // ScheduleWakeup turn (or the wake itself) overrides it cleanly.
  // See #1091.
  if (next.nextWakeupAt) {
    const wakeAt = new Date(next.nextWakeupAt).getTime();
    if (!Number.isNaN(wakeAt) && Date.now() >= wakeAt) {
      next.nextWakeupAt = null;
      next.nextWakeupReason = null;
    }
  }
  // A monitor has no fire time to gate on. Unlike a wakeup it never
  // self-fires a prompt, so any UserPromptSent reaching here is the user
  // taking over: clear the "monitoring" badge unconditionally.
  next.monitorArmed = false;
  next.monitorWorkSeen = false;
  next.monitorDescription = null;
  // Any pending context-primer offer is consumed once the user submits
  // a new prompt; the recovery affordance is one-shot.
  next.contextPrimerAvailable = null;
}

/** Pure reducer. Returns a new state; never mutates the input.
 *  Drops frames whose seq is not strictly greater than `state.lastSeq`
 *  so reconnect/replay can re-deliver buffered frames without
 *  double-applying them (duplicate tool cards, doubled message
 *  chunks, etc.). */
export function applyEvent(state: AcpState, frame: AcpFrame): AcpState {
  if (frame.seq <= state.lastSeq) {
    return state;
  }
  const next = { ...state, lastSeq: frame.seq };
  const event = frame.event;
  if (typeof event === "string") {
    // The phases themselves (thinking, compacting) and the state a context
    // boundary invalidates (plan, mode, pending cards) are server-owned since
    // Tier 1.2. What stays is the cost baseline, which the server does not
    // model: the agent reports session-lifetime cumulative cost, so each
    // boundary snapshots it to keep the footer reading "since the most recent
    // boundary". See #1354 / #1109.
    if (event === "ThinkingStarted") {
      // Agent-initiated work with no prompt behind it still opens a turn.
      // Mirrors `AcpState::apply_event`; see closeTurn's note on why the raw
      // fold tracks these edges at all.
      next.serverTurnActive = true;
      next.turnActive = true;
    }
    if (event === "ConversationCompacted" || event === "SessionCleared") {
      const priorUsage = state.sessionUsage?.cost?.amount ?? 0;
      const priorBaseline = state.usageBaseline?.cost ?? 0;
      next.usageBaseline = { cost: priorUsage + priorBaseline };
      next.sessionUsage = null;
    }
    return next;
  }
  if ("ToolCallStarted" in event) {
    // A tool call after the monitor armed means the monitor fired and the
    // agent is acting on it; gate the badge clear on the next Stopped. See
    // #2325. The in-flight pointer itself is server-owned (Tier 1.2).
    if (next.monitorArmed) {
      next.monitorWorkSeen = true;
    }
    return next;
  }
  if ("UsageUpdated" in event) {
    // claude-agent-acp keeps reporting session-lifetime cumulative cost via
    // UsageUpdate; subtract the baseline captured at the most recent
    // boundary so the composer footer reads "since the most recent
    // boundary." See #1354.
    const incoming = event.UsageUpdated.usage;
    // Bandaid for upstream claude-agent-acp #596: latch the largest window
    // learned this session so the footer doesn't flicker 200k <-> 1M. See
    // the note in the original reducer.
    const size = Math.max(incoming.size, next.sessionUsage?.size ?? 0);
    // Re-arm the compaction reminder on the first snapshot after a context
    // boundary. Every boundary nulls sessionUsage, so a null previous
    // snapshot IS the boundary signal. See #3253.
    if (next.compactionReminderDismissed && next.sessionUsage === null) {
      next.compactionReminderDismissed = null;
    }
    if (next.usageBaseline && incoming.cost) {
      const rebasedAmount = Math.max(0, incoming.cost.amount - next.usageBaseline.cost);
      const rebasedCost = {
        amount: rebasedAmount,
        currency: incoming.cost.currency,
      };
      next.sessionUsage = {
        used: incoming.used,
        size,
        cost: rebasedCost,
      };
    } else {
      next.sessionUsage = { used: incoming.used, size, cost: incoming.cost };
    }
    return next;
  }
  if ("CurrentModeChanged" in event) {
    // The mode itself is server-owned (Tier 1.2); the switch actually
    // landing is what makes a prior failure notice stale.
    next.modeSwitchFailed = null;
    return next;
  }
  if ("ModeSwitchFailed" in event) {
    next.modeSwitchFailed = {
      modeId: event.ModeSwitchFailed.mode_id,
      reason: event.ModeSwitchFailed.reason,
      at: new Date().toISOString(),
    };
    return next;
  }
  if ("ConfigOptionsUpdated" in event) {
    const options = event.ConfigOptionsUpdated.options;
    // A model change moves the context window, so drop the latched usage
    // window and relearn it for the new model. See the UsageUpdated arm.
    const priorModel = next.configOptions.find((o) => o.category === "model")?.current_value;
    const nextModel = options.find((o) => o.category === "model")?.current_value;
    if (priorModel !== undefined && nextModel !== undefined && priorModel !== nextModel) {
      next.sessionUsage = null;
    }
    next.configOptions = options;
    // The snapshot is authoritative, so any in-flight pending click resolves
    // here. A rejected change comes through ConfigOptionSwitchFailed instead.
    next.pendingConfigOption = null;
    // Auto-dismiss a stale switch-failed notice when this snapshot confirms
    // the originally-requested value.
    if (next.configOptionSwitchFailed) {
      const failure = next.configOptionSwitchFailed;
      const confirmed = options.some((opt) => opt.id === failure.configId && opt.current_value === failure.value);
      if (confirmed) {
        next.configOptionSwitchFailed = null;
      }
    }
    return next;
  }
  if ("ConfigOptionSwitchFailed" in event) {
    next.configOptionSwitchFailed = {
      configId: event.ConfigOptionSwitchFailed.config_id,
      value: event.ConfigOptionSwitchFailed.value,
      reason: event.ConfigOptionSwitchFailed.reason,
      at: new Date().toISOString(),
    };
    next.pendingConfigOption = null;
    return next;
  }
  if ("Stopped" in event) {
    // Final marker. Every in-turn phase it used to clear (in-flight tool,
    // thinking, cancelling, compacting) is server-owned since Tier 1.2, and
    // closing the turn joined them in #3417: whether this `Stopped` ends the
    // turn or only one of several prompts steered into it is the daemon's
    // call, and the `reduced_state` frame that follows this event on the WS
    // carries the answer. The raw edge is mirrored for the history fold; see
    // closeTurn.
    next.cancelEscalatesAt = null;
    closeTurn(next);
    // Clear the "monitoring" badge once the monitor has fired and that turn
    // ends. See #2325.
    if (next.monitorArmed && next.monitorWorkSeen) {
      next.monitorArmed = false;
      next.monitorWorkSeen = false;
      next.monitorDescription = null;
    }
    if (event.Stopped.reason === "user_stopped") {
      next.workerStopped = true;
      next.workerRestarting = false;
      next.agentUnresponsive = false;
      next.agentOrphaned = false;
    } else if (event.Stopped.reason === "restart_pending") {
      next.workerRestarting = true;
      next.workerStopped = false;
      next.agentUnresponsive = false;
      next.agentOrphaned = false;
    } else if (event.Stopped.reason === "agent_unresponsive") {
      // Cancel-escalation watchdog fired: reuse workerRestarting's composer
      // lockdown; agentUnresponsive lets the banner render the cause. See
      // #1196.
      next.workerRestarting = true;
      next.workerStopped = false;
      next.agentUnresponsive = true;
      next.agentOrphaned = false;
    } else if (event.Stopped.reason === "prompt_orphaned") {
      // Silent-orphan watchdog fired. Distinct banner copy from
      // agent_unresponsive; both reuse the workerRestarting lockdown. See
      // #1240.
      next.workerRestarting = true;
      next.workerStopped = false;
      next.agentUnresponsive = false;
      next.agentOrphaned = true;
    } else if (event.Stopped.reason === "idle_auto_stop") {
      // The reconciler reaped the worker for inactivity and marked the
      // session dormant (#1689). Recoverable without a reconnect: the next
      // prompt POST wakes it.
      next.workerIdleStopped = true;
      next.workerStopped = false;
      next.workerRestarting = false;
    } else if (event.Stopped.reason === "rate_limit_exhausted_retries") {
      // Auto-resume gave up re-delivering the interrupted prompt after its
      // cap and parked the session (#3688). RESUME NOW and a fresh prompt
      // both recover; until then the banner explains the park.
      next.rateLimitRetriesExhausted = true;
    }
    return next;
  }
  if ("IncompatibleAgent" in event) {
    // The structured view refused to enter the session because the adapter
    // failed the per-adapter compatibility check. The structured payload
    // powers the StartupErrorScreen.
    next.incompatibleAgent = event.IncompatibleAgent.detail;
    next.agentUnresponsive = false;
    return next;
  }
  if ("AgentStartupError" in event) {
    next.startupError = event.AgentStartupError.message;
    // A failed respawn supersedes any in-progress unresponsive escalation.
    next.agentUnresponsive = false;
    closeTurn(next);
    return next;
  }
  if ("PromptRuntimeError" in event) {
    next.lastError = event.PromptRuntimeError.message;
    closeTurn(next);
    return next;
  }
  if ("PromptCapabilities" in event) {
    const c = event.PromptCapabilities;
    next.promptCapabilities = {
      image: c.image,
      audio: c.audio,
      embeddedContext: c.embedded_context,
      steering: c.steering ?? false,
    };
    return next;
  }
  if ("UserPromptSent" in event) {
    // Control-only: the transcript row is server-owned. This is the daemon
    // acknowledging the prompt, so it settles the matching optimistic id by
    // the minted `prompt_id` (#3173) and opens the turn. Opening it here
    // rather than waiting for the `reduced_state` frame that follows is the
    // one place the client mirrors a daemon turn edge: without it, settling
    // the id would drop `turnActive` for the single frame between the two.
    // The following `reduced_state` stays authoritative.
    const pid = event.UserPromptSent.prompt_id;
    const wasInflight = pid != null && pid.length > 0 && next.inflightPromptIds.includes(pid);
    if (wasInflight) {
      next.inflightPromptIds = next.inflightPromptIds.filter((id) => id !== pid);
    } else {
      next.promptSeq += 1;
    }
    next.serverTurnActive = true;
    next.turnActive = true;
    if (!isSteeredContinuation(state)) {
      applyNewTurnResets(next);
    }
    return next;
  }
  if ("UserDiffCommentsPrompt" in event) {
    // The "Send diff comments" dialog posts directly with no optimistic
    // overlay, so there is no id to settle. The typed row is server-owned.
    next.promptSeq += 1;
    next.serverTurnActive = true;
    next.turnActive = true;
    if (!isSteeredContinuation(state)) {
      applyNewTurnResets(next);
    }
    return next;
  }
  if ("AcpSessionAssigned" in event) {
    // Persistence breadcrumb plus "agent connection is alive again" signal.
    // Clear sticky error / worker banners so the UI heals once a respawn
    // completes the handshake.
    next.startupError = null;
    next.lastError = null;
    next.incompatibleAgent = null;
    next.workerStopped = false;
    next.workerRestarting = false;
    next.workerIdleStopped = false;
    next.agentUnresponsive = false;
    next.agentOrphaned = false;
    return next;
  }
  if ("RateLimitAutoResumed" in event) {
    // A resume fired (manual RESUME NOW or the auto-resume pass before it
    // gave up): the worker is coming back, so the exhausted-retries park is
    // over until a new streak parks it again (#3688).
    next.rateLimitRetriesExhausted = false;
    return next;
  }
  if ("SessionContextReset" in event) {
    // session/load failed and the agent fell back to session/new; its
    // context window is empty. Clear the now-stale usage hint and baseline.
    next.sessionUsage = null;
    next.usageBaseline = null;
    // Suppress the primer offer on a session that never saw a user prompt
    // (a 0-prompt session's session/load failure is expected). The visible
    // reset row is server-owned; here we only gate the primer affordance.
    // `promptSeq` counts prompts applied before this event (frames are applied
    // in seq order), so zero is the "never had a prompt" signal.
    if (state.promptSeq <= 0) {
      return next;
    }
    // Offer the opt-in primer affordance; one-shot, cleared by the next
    // UserPromptSent via applyNewTurnResets. See #1004 / #1110.
    next.contextPrimerAvailable = {
      resetSeq: frame.seq,
      reason: event.SessionContextReset.reason || "Conversation context reset; agent transcript was unavailable.",
    };
    return next;
  }
  if ("WakeupScheduled" in event) {
    next.nextWakeupAt = event.WakeupScheduled.at;
    next.nextWakeupReason = event.WakeupScheduled.reason ?? null;
    return next;
  }
  if ("MonitorArmed" in event) {
    next.monitorArmed = true;
    // Reset the fired-work gate: work only counts once it follows THIS arm.
    next.monitorWorkSeen = false;
    next.monitorDescription = event.MonitorArmed.description ?? null;
    return next;
  }
  if ("CancelRequested" in event) {
    // aoe sent session/cancel and armed the escalation watchdog; the turn is
    // still active. Surface "Stopping..." and the escalation deadline. See
    // #1727.
    // The phase is server-owned; the escalation deadline is not modelled
    // there, so the honest countdown still comes off the event.
    next.cancelEscalatesAt = event.CancelRequested.escalates_at;
    return next;
  }
  if ("AgentSwitched" in event) {
    // ACP backend handoff completed. Drop everything tied to the prior
    // backend so the composer/footer don't keep showing stale usage, mode
    // pills, or an in-flight tool while talking to the new agent. The
    // transcript divider row is server-owned. See #1282.
    const { from, to, reason } = event.AgentSwitched;
    const now = new Date().toISOString();
    // Everything the new backend re-advertises (agent, rate limit, in-flight
    // tool, pending cards, commands, modes, plan, mode) is dropped
    // server-side; what stays here is the client's own cost bookkeeping and
    // the banners the server does not model.
    next.sessionUsage = null;
    // The new backend reports its own cumulative cost from zero. See #1354.
    next.usageBaseline = null;
    next.startupError = null;
    next.lastAgentSwitch = { from, to, reason, at: now };
    // The switch path emits Stopped { user_stopped } just before this; clear
    // the worker banners eagerly so they stay hidden through the new agent's
    // handshake.
    next.workerStopped = false;
    next.workerRestarting = false;
    next.agentUnresponsive = false;
    next.rateLimitRetriesExhausted = false;
    // Per-adapter selectors belong to the previous backend. See #1403.
    next.configOptions = [];
    next.configOptionSwitchFailed = null;
    next.pendingConfigOption = null;
    return next;
  }
  if ("PromptRejected" in event) {
    // Daemon refused the follow-up prompt because another session/prompt was
    // still in flight. Show a Retry pill. See #1196.
    const entry: RejectedPrompt = {
      id: `rejected-${frame.seq}`,
      text: event.PromptRejected.text,
      reason: event.PromptRejected.reason,
      rejectedAt: new Date().toISOString(),
    };
    const REJECTED_PROMPTS_CAP = 5;
    next.rejectedPrompts = [...next.rejectedPrompts, entry].slice(-REJECTED_PROMPTS_CAP);
    // The composer's own optimistic marker is settled by the POST that got
    // the rejection; this closes the turn the daemon closed. See closeTurn.
    closeTurn(next);
    return next;
  }
  if ("BackgroundAgentLaunched" in event) {
    const e = event.BackgroundAgentLaunched;
    const record: BackgroundAgent = {
      agentId: e.agent_id,
      toolCallId: e.tool_call_id,
      description: e.description,
      prompt: e.prompt,
      model: e.model,
      status: "running",
      startedAt: e.started_at,
      endedAt: null,
      toolCount: 0,
      tools: [],
      lastTool: null,
      lastText: null,
      result: null,
      warning: null,
    };
    // Idempotent on replay: replace any existing record for this agent.
    const i = next.backgroundAgents.findIndex((a) => a.agentId === e.agent_id);
    next.backgroundAgents =
      i >= 0 ? next.backgroundAgents.map((a, idx) => (idx === i ? record : a)) : [...next.backgroundAgents, record];
    return next;
  }
  if ("BackgroundAgentProgress" in event) {
    const e = event.BackgroundAgentProgress;
    next.backgroundAgents = next.backgroundAgents.map((a) => {
      if (a.agentId !== e.agent_id) return a;
      // A terminal record never reopens to running.
      if (a.status === "completed" || a.status === "detached" || a.status === "error") return a;
      return {
        ...a,
        status: e.status,
        toolCount: e.tool_count,
        tools: e.tools && e.tools.length > 0 ? e.tools : a.tools,
        endedAt: e.status === "stalled" ? (a.endedAt ?? e.at) : e.status === "running" ? null : a.endedAt,
        lastTool: e.last_tool ?? a.lastTool,
        lastText: e.last_text ?? a.lastText,
      };
    });
    return next;
  }
  if ("BackgroundAgentCompleted" in event) {
    const e = event.BackgroundAgentCompleted;
    next.backgroundAgents = next.backgroundAgents.map((a) =>
      a.agentId === e.agent_id
        ? {
            ...a,
            status: e.status,
            endedAt: e.ended_at,
            tools: e.tools && e.tools.length > 0 ? e.tools : a.tools,
            result: e.result ?? a.result,
            warning: e.warning ?? a.warning,
          }
        : a,
    );
    return next;
  }
  // DiffEmitted, RawAgentUpdate, TodoListUpdated, ConversationSummary,
  // ToolCallContent, and anything else carry no control state (their
  // transcript rows, where any, are server-owned): pass through unchanged.
  return next;
}

/** Fold a self-contained run of frames from an empty state. Used by the
 *  recent-first load to reduce an OLDER history page in isolation (so the
 *  page's activity rows can be prepended without disturbing the live
 *  reducer's optimistic / queue / approval state, which is not a pure
 *  fold of the frame log), and to project the handshake snapshot. The
 *  caller is responsible for the frames being a clean unit; backward
 *  paging guarantees each page starts at a user-turn boundary. See #2236. */
export function reduceFrames(frames: AcpFrame[]): AcpState {
  return frames.reduce(applyEvent, emptyAcpState());
}

/** Adopt the daemon's folded control state (Tier 1.2). Every field here is
 *  the server's verbatim, so a client derivation for any of them would be a
 *  second source of truth; the arms that used to build them are gone.
 *
 *  Deliberately does NOT touch `lastSeq`: raw frames still drive the rest of
 *  the reducer, and their seq dedupe is what keeps replay idempotent.
 *
 *  The one piece of optimism kept is approvals and elicitations the user just
 *  answered: the card clears on the resolve POST rather than waiting for the
 *  broadcast (#1821), and without the filter the very next frame would paint
 *  it straight back. A nonce is forgotten once the server stops listing it,
 *  so a later request reusing it is not swallowed. */
export function applyReducedState(state: AcpState, reduced: ReducedState, unchanged: string[] = []): AcpState {
  // Cold fields the server omitted because this socket already holds them
  // (a ~30 KB command list re-sent after every event dominated the frame).
  // They arrive as empty defaults, so adopting them would blank the pickers.
  const holds = (field: string) => unchanged.includes(field);
  const stillPending = new Set<string>([
    ...reduced.pending_approvals.map((a) => a.nonce),
    ...reduced.pending_elicitations.map((e) => e.nonce),
  ]);
  const locallyResolved = state.locallyResolved.filter((nonce) => stillPending.has(nonce));
  const resolved = new Set(locallyResolved);
  return {
    ...state,
    agent: reduced.agent,
    model: reduced.model,
    mode: reduced.mode,
    plan: reduced.current_plan,
    inFlightTool: reduced.in_flight_tool,
    pendingApprovals: reduced.pending_approvals.filter((a) => !resolved.has(a.nonce)),
    pendingElicitations: reduced.pending_elicitations.filter((e) => !resolved.has(e.nonce)),
    thinking: reduced.thinking != null,
    rateLimit: reduced.rate_limit,
    availableCommands: holds("available_commands") ? state.availableCommands : reduced.available_commands,
    availableModes: holds("available_modes") ? state.availableModes : reduced.available_modes,
    currentModeId: reduced.current_mode_id,
    // Steady-state turn truth. A false frame cannot suppress a prompt whose
    // POST is still unacknowledged, so an unrelated frame arriving in that
    // window does not flicker the composer back to idle. See #3417.
    serverTurnActive: reduced.turn_active,
    turnActive: deriveTurnActive({
      serverTurnActive: reduced.turn_active,
      inflightPromptIds: state.inflightPromptIds,
    }),
    cancelling: reduced.cancelling,
    compacting: reduced.compacting,
    locallyResolved,
  };
}

/** Merge a duplicate `ToolCallStarted` into the existing row's payload
 *  without clobbering richer data with a sparser frame. A permission
 *  start (#1713) carries empty args and `kind: "other"`; a later real
 *  start frame for the same id must win, but a real start that arrives
 *  first must not be overwritten by the sparse permission start. */
function mergeToolStart(prev: ToolCall, incoming: ToolCall): ToolCall {
  const startedAt =
    !prev.started_at ||
    (incoming.started_at.length > 0 && Date.parse(incoming.started_at) > Date.parse(prev.started_at))
      ? incoming.started_at
      : prev.started_at;

  return {
    ...prev,
    ...incoming,
    name: incoming.name.length > 0 ? incoming.name : prev.name,
    // Keep whichever start frame carried a non-empty wire identity; a
    // sparse permission start (#1713) has an empty name/raw_name and must
    // not clobber the real start's `raw_name`. See #3070.
    raw_name: incoming.raw_name && incoming.raw_name.length > 0 ? incoming.raw_name : prev.raw_name,
    kind: incoming.kind && incoming.kind !== "other" ? incoming.kind : prev.kind,
    args_preview: incoming.args_preview.trim().length > 0 ? incoming.args_preview : prev.args_preview,
    started_at: startedAt,
    diffs: incoming.diffs && incoming.diffs.length > 0 ? incoming.diffs : prev.diffs,
    parent_tool_call_id: incoming.parent_tool_call_id ?? prev.parent_tool_call_id,
    memory_recall: incoming.memory_recall ?? prev.memory_recall,
  };
}

/** Prepend an older history page's rows ahead of the loaded tail, deduping
 *  any `tool_start` whose `toolCallId` already exists in the tail. A tool
 *  call split across the page seam (its `ToolCallStarted` in this older
 *  page, its `ToolCallCompleted` already in the tail) left a synthesized
 *  placeholder start in `tailRows` (see synthToolStartRow / #1713). Without
 *  a cross-page merge the real start would prepend as a second row with the
 *  same id, and two assistant-ui `tool-call` parts sharing a `toolCallId`
 *  make `useResources` throw "Duplicate key" and crash the panel (#2711).
 *  Merge the real start into the existing row in place (real name/kind/args
 *  and its earlier `started_at` win) and drop the duplicate. Also covers a
 *  plain frame overlap at the seam. */
export function mergePrependedActivity(olderRows: ActivityRow[], tailRows: ActivityRow[]): ActivityRow[] {
  const startIndexById = new Map<string, number>();
  tailRows.forEach((row, i) => {
    if (row.kind === "tool_start" && row.toolCallId) startIndexById.set(row.toolCallId, i);
  });
  if (startIndexById.size === 0) return olderRows.concat(tailRows);

  let tail = tailRows;
  const prepended: ActivityRow[] = [];
  for (const row of olderRows) {
    const idx = row.kind === "tool_start" && row.toolCallId ? startIndexById.get(row.toolCallId) : undefined;
    if (idx === undefined) {
      prepended.push(row);
      continue;
    }
    const existing = tail[idx];
    if (existing && existing.kind === "tool_start" && existing.tool && row.tool) {
      const merged = mergeToolStart(existing.tool, row.tool);
      // Keep the real start's timestamp; the synth placeholder carried the
      // completion time, which would zero out the duration label (#1060).
      if (row.tool.started_at) merged.started_at = row.tool.started_at;
      tail = tail.slice();
      tail[idx] = { ...existing, tool: merged, text: merged.name, at: merged.started_at };
    }
  }
  return prepended.concat(tail);
}

/** Append an optimistic `elicitation_answered` overlay row recording the
 *  user's just-picked answers, keyed by `elicitation-<nonce>` and deduped by
 *  id. The authoritative row is server-owned (the daemon folds
 *  `ElicitationResolved` into the same-id transcript row); this overlay gives
 *  instant feedback and is dropped once that server row lands. No row for
 *  empty answers (skip / cancel / teardown). See #2209. */
export function appendElicitationAnswerRow(
  rows: ActivityRow[],
  nonce: string,
  answers: ElicitationAnswer[],
): ActivityRow[] {
  const id = `elicitation-${nonce}`;
  if (answers.length === 0 || rows.some((r) => r.id === id)) return rows;
  return rows.concat({
    id,
    kind: "elicitation_answered",
    text: answers.map((a) => `${a.question}: ${a.answer}`).join("\n"),
    elicitationAnswers: answers,
    at: new Date().toISOString(),
  });
}

/** Daemon turn state plus prompt POSTs whose acknowledgements have not arrived.
 *  Mid-turn steering can complete several prompts with one terminal event, so
 *  prompt counters cannot derive this reliably. */
export function deriveTurnActive(state: Pick<AcpState, "serverTurnActive" | "inflightPromptIds">): boolean {
  return state.serverTurnActive || state.inflightPromptIds.length > 0;
}

/** Close the turn from a raw event, mirroring `AcpState::apply_event`'s own
 *  `turn_active = false` edges (`src/acp/state.rs`).
 *
 *  The `reduced_state` frame the daemon pushes after every event is still
 *  authoritative, so this looks redundant on the WS path. It is not on the
 *  history path: `GET /acp/replay` serves raw events with no `reduced_state`
 *  alongside, so a cold open that folded only the opening edges would paint a
 *  spinner over a session that finished hours ago until the WS connect
 *  snapshot corrected it. Mirroring the same seven edges the daemon uses
 *  keeps the raw fold self-consistent. See #3417. */
function closeTurn(next: AcpState): void {
  next.serverTurnActive = false;
  next.turnActive = deriveTurnActive(next);
}

/** Whether the structured view should show the compaction reminder.
 *  Single source of truth for the gate so the banner and its tests
 *  cannot drift.
 *
 *  Capability gates the whole reminder, not just its button: telling a
 *  user to run a command their agent never advertised is noise. A
 *  zero-size window means the agent has not reported a real window yet,
 *  so there is no percentage to compare; `used > size` (which some agents
 *  report transiently) is past any legal threshold and counts. See #3253.
 */
export function isCompactionReminderDue(
  state: Pick<AcpState, "sessionUsage" | "compacting" | "compactionReminderDismissed" | "availableCommands">,
  prefs: { compactionReminder: boolean; compactionReminderPercent: number },
): boolean {
  if (!prefs.compactionReminder) return false;
  if (state.compacting || state.compactionReminderDismissed) return false;
  const usage = state.sessionUsage;
  if (!usage || !Number.isFinite(usage.used) || !Number.isFinite(usage.size) || usage.size <= 0) {
    return false;
  }
  if (!state.availableCommands.some((c) => c.name === "compact")) return false;
  return (usage.used / usage.size) * 100 >= prefs.compactionReminderPercent;
}

/** Normalise a partial AcpState so the turn state is populated. Used by
 *  the localStorage loader: an entry persisted before #3417 carries the
 *  retired `pendingUserPromptSeq` / `lastStoppedSeq` counters and no
 *  `serverTurnActive`, so we seed the latter from the cached `turnActive`
 *  boolean as a warm hint. The WS connect snapshot replaces it with daemon
 *  truth a moment later. In-flight prompt ids are request-local and always
 *  start empty: after a reload there is no POST left to acknowledge them. */
export function normaliseTurnState(
  state: AcpState & {
    oldestSeq?: number;
    serverTurnActive?: boolean;
    promptSeq?: number;
    rejectedPrompts?: RejectedPrompt[];
    agentUnresponsive?: boolean;
    agentOrphaned?: boolean;
    usageBaseline?: { cost: number } | null;
    configOptions?: ConfigOptionDescriptor[];
    configOptionSwitchFailed?: ConfigOptionSwitchFailure | null;
    pendingConfigOption?: { configId: string; value: string } | null;
    compactionReminderDismissed?: SessionUsage | null;
  },
): AcpState {
  const serverTurnActive =
    typeof state.serverTurnActive === "boolean" ? state.serverTurnActive : state.turnActive === true;
  // A hydrate that already has prompt rows already had prompts; a cold entry
  // re-folds its history and counts them on the way through.
  const promptSeq =
    typeof state.promptSeq === "number" && Number.isFinite(state.promptSeq)
      ? Math.max(0, Math.floor(state.promptSeq))
      : (state.activity ?? []).filter((r) => r.kind === "user_prompt").length;
  // Pre-#1196 persisted entries lack rejectedPrompts / agentUnresponsive;
  // backfill so the reducer and renderers see well-typed values instead
  // of `undefined` (which crashes RejectedPromptsStrip's `.length` read).
  const rejectedPrompts = Array.isArray(state.rejectedPrompts) ? state.rejectedPrompts : [];
  const agentUnresponsive = typeof state.agentUnresponsive === "boolean" ? state.agentUnresponsive : false;
  // Pre-#1240 persisted entries lack agentOrphaned; backfill to false
  // so the reducer and renderers see a well-typed value instead of
  // `undefined`.
  const agentOrphaned = typeof state.agentOrphaned === "boolean" ? state.agentOrphaned : false;
  // Pre-#1354 persisted entries lack usageBaseline; backfill to null
  // so the UsageUpdated reducer's `next.usageBaseline && ...` check
  // sees a well-typed value. The baseline stays null until the next
  // SessionCleared / ConversationCompacted, which matches the
  // pre-fix behaviour for that one session; subsequent /clear events
  // start subtracting normally.
  const usageBaseline = state.usageBaseline === undefined ? null : state.usageBaseline;
  // Pre-#1403 persisted entries lack the config-option trio.
  const configOptions = Array.isArray(state.configOptions) ? state.configOptions : [];
  const configOptionSwitchFailed = state.configOptionSwitchFailed === undefined ? null : state.configOptionSwitchFailed;
  const pendingConfigOption = state.pendingConfigOption === undefined ? null : state.pendingConfigOption;
  // Pre-#3253 persisted entries lack the compaction-reminder dismissal;
  // backfill to null so a warm hydrate starts armed rather than reading
  // `undefined` as "not dismissed" by luck.
  const compactionReminderDismissed =
    state.compactionReminderDismissed === undefined ? null : state.compactionReminderDismissed;
  // Pre-#2236 persisted entries lack oldestSeq; backfill to 0 (nothing
  // older loaded) so the recent-first `before=<oldestSeq>` paging contract
  // never sees undefined on a warm hydrate.
  const oldestSeq =
    typeof state.oldestSeq === "number" && Number.isFinite(state.oldestSeq)
      ? Math.max(0, Math.floor(state.oldestSeq))
      : 0;
  return {
    ...state,
    oldestSeq,
    rejectedPrompts,
    agentUnresponsive,
    agentOrphaned,
    usageBaseline,
    configOptions,
    configOptionSwitchFailed,
    pendingConfigOption,
    compactionReminderDismissed,
    serverTurnActive,
    promptSeq,
    inflightPromptIds: [],
    turnActive: serverTurnActive,
  };
}

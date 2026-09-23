// Structured view wire types mirroring `src/acp/state.rs`; permissive so new Rust variants don't break the UI.

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
  /** Wire tool identity from `ToolCallStarted`; never retitled, so classification can key on it. */
  raw_name?: string;
  /** ACP ToolKind, lowercased. */
  kind: string;
  args_preview: string;
  started_at: string; // ISO-8601 from chrono
  /** Parent `Task` call for sub-agent tool calls. */
  parent_tool_call_id?: string;
  memory_recall?: MemoryRecall | null;
  /** Structured diffs from ACP `ToolCallContent::Diff` (Codex `apply_patch`); preferred over the args shape. */
  diffs?: DiffPreview[] | null;
}

export interface MemoryRecall {
  /** "recall" (file list) or "synthesize" (text body). */
  mode: string;
  paths?: string[];
  synthesized_text?: string | null;
}

export interface DiffPreview {
  path: string;
  old_text?: string | null;
  new_text?: string | null;
  created_at: string;
}

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
      data?: string | null;
    };

export interface RateLimitInfo {
  status: string;
  /** Null when the agent never attributed a reset to the rejected window; show `status` instead. */
  resets_at: string | null;
  kind: string;
}

export interface SessionUsage {
  used: number;
  size: number;
  /** Cumulative session cost, when reported. */
  cost?: { amount: number; currency: string } | null;
}

export interface AvailableCommand {
  name: string;
  description: string;
  /** The command takes free-form arguments after the name. */
  accepts_input: boolean;
}

/** Unknown categories arrive as bare strings (Rust `Other` is untagged). */
export type ConfigOptionCategory = "mode" | "model" | "thought_level" | (string & {});

export interface ConfigOptionChoice {
  value: string;
  name: string;
  description?: string | null;
}

/** Each `ConfigOptionsUpdated` replaces the whole list. */
export interface ConfigOptionDescriptor {
  id: string;
  name: string;
  description?: string | null;
  category: ConfigOptionCategory;
  current_value: string;
  options: ConfigOptionChoice[];
}

export interface ConfigOptionSwitchFailure {
  configId: string;
  value: string;
  reason: string;
  at: string;
}

export type ApprovalOptionKind = "allow_once" | "allow_always" | "reject_once" | "reject_always";

export interface ApprovalOption {
  option_id: string;
  name: string;
  kind: ApprovalOptionKind;
}

export interface Approval {
  nonce: string;
  tool_call: ToolCall;
  destructive: boolean;
  /** Absent on approvals replayed from older event logs. */
  options?: ApprovalOption[];
  /** The options are answers rather than a permission vocabulary; post back the picked `option_id`. */
  choice?: boolean;
  requested_at: string;
  resolved?: {
    decision: ApprovalDecision;
    message?: string | null;
    resolved_at: string;
  } | null;
}

export type ElicitationFieldKind = "free_text" | "single_select" | "multi_select" | "number" | "integer" | "boolean";

export interface ElicitationOption {
  value: string;
  label: string;
  description?: string | null;
}

/** Mirror of the untagged Rust `AnswerValue`. */
export type AnswerValue = string | string[] | number | boolean;

export interface ElicitationQuestion {
  field_key: string;
  title?: string | null;
  description?: string | null;
  required: boolean;
  kind: ElicitationFieldKind;
  options: ElicitationOption[];
  min_items?: number | null;
  max_items?: number | null;
  min_length?: number | null;
  max_length?: number | null;
  pattern?: string | null;
  /** Format annotation (`email`, `uri`, `date`, ...), mapped to an input type. */
  format?: string | null;
  minimum?: number | null;
  maximum?: number | null;
  default?: AnswerValue | null;
}

/** Mirror of `Elicitation` in src/acp/elicitations.rs. */
export interface Elicitation {
  nonce: string;
  message: string;
  title?: string | null;
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

export type ElicitationResolution =
  | { action: "accept"; answers: Record<string, AnswerValue> }
  | { action: "decline" }
  | { action: "cancel" };

export interface ElicitationAnswer {
  question: string;
  answer: string;
}

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

// Written as an escape so the em dash never appears literally in source; mirrors src/acp/elicitations.rs.
const OPTION_DESC_SEP = " \u2014 ";

// MCP forms send a machine token as the value; AskUserQuestion sends the label itself, so keep it bare.
function selectLabel(question: ElicitationQuestion, raw: string): string {
  const opt = question.options.find((o) => o.value === raw);
  if (!opt) return raw;
  return opt.label.startsWith(`${raw}${OPTION_DESC_SEP}`) ? raw : opt.label;
}

function renderAnswerValue(question: ElicitationQuestion, value: AnswerValue): string {
  if (typeof value === "boolean") return value ? "Yes" : "No";
  if (Array.isArray(value)) return value.map((v) => selectLabel(question, v)).join(", ");
  if (typeof value === "string") return selectLabel(question, value);
  return String(value);
}

/** Mirrors `summarize_answers` in src/acp/elicitations.rs so the optimistic row matches the server's. */
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

/** Mirror of `StartupErrorDetail` in src/acp/state.rs. */
export type IncompatibleAgentDetail =
  | {
      kind: "incompatible_agent_version";
      package_name: string;
      installed: string;
      required: string;
      install_command: string;
      /** The daemon can `npm install -g` this agent itself. */
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

// One variant per Event::* in src/acp/state.rs, externally tagged.
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
        /** Final text from ACP `ToolCallUpdate.fields.content`; empty when none. */
        content: string;
        output?: ToolOutputBlock[];
        /** Server clock at completion, so durations survive replay. Absent on older events. */
        completed_at?: string;
        /** Synchronous launch of an async sub-agent whose real work never reports back on this stream. */
        async_subagent?: boolean;
      };
    }
  | {
      /** Latest full content snapshot (a replacement, not an append). */
      ToolCallContent: { tool_call_id: string; content: string };
    }
  | {
      /** Late inputs/title for a started tool call; claude-agent-acp fills in the command here. */
      ToolCallUpdated: {
        tool_call_id: string;
        title: string | null;
        args_preview: string | null;
        /** Re-stamped when the tool actually starts running, so durations exclude scheduling time. */
        started_at?: string | null;
        /** A non-empty list replaces the card's diffs; null leaves them untouched. */
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
        /** Client-minted id echoed back so the optimistic row reconciles by id. */
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
        /** Absent on older events; treat as false. */
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

export interface PromptAttachmentRefWire {
  id: string;
  kind: PromptAttachmentKind;
  mime_type: string;
  name?: string;
  size: number;
}

export type PromptAttachmentKind = "image" | "audio" | "resource";

export interface PromptCapabilities {
  image: boolean;
  audio: boolean;
  embeddedContext: boolean;
  /** Re-emitted on every connect so it cannot go stale after a respawn. */
  steering: boolean;
}

export interface PromptAttachmentInput {
  kind: PromptAttachmentKind;
  mimeType: string;
  name?: string;
  /** Standard base64, no `data:` URL prefix. */
  dataB64: string;
}

/** `url` is the replay endpoint, or a local object URL for an unconfirmed optimistic row. */
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

/** Fields this client adopts from the daemon's folded `AcpState`. */
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
  promptCapabilities: PromptCapabilities | null;
  plan: Plan | null;
  inFlightTool: ToolCall | null;
  pendingApprovals: Approval[];
  pendingElicitations: Elicitation[];
  /** Answered here but still in the daemon's pending list; see {@link applyReducedState}. */
  locallyResolved: string[];
  thinking: boolean;
  rateLimit: RateLimitInfo | null;
  sessionUsage: SessionUsage | null;
  /** Cost at the latest context boundary, subtracted from the agent's lifetime total. */
  usageBaseline: { cost: number } | null;
  /** Usage when the compaction reminder was dismissed, or null while armed. */
  compactionReminderDismissed: SessionUsage | null;
  activity: ActivityRow[];
  /** Unpersisted optimistic rows, removed when the server row with the same id arrives. */
  optimisticRows: ActivityRow[];
  /** Frames with `seq` at or below this are dropped, so replays are idempotent. */
  lastSeq: number;
  /** Lowest loaded seq; older history pages load with `?before=<oldestSeq>`. */
  oldestSeq: number;
  lagged: boolean;
  startupError: string | null;
  /** Adapter compatibility failure; replaces the session view with `StartupErrorScreen`. */
  incompatibleAgent: IncompatibleAgentDetail | null;
  lastError: string | null;
  /** Derived from `serverTurnActive || inflightPromptIds.length > 0`; never written directly. */
  turnActive: boolean;
  /** The daemon's `turn_active`; only it knows whether a mid-turn prompt was steered into the running turn. */
  serverTurnActive: boolean;
  /** Prompt ids POSTed but not yet echoed or failed; covers the POST-to-echo gap. */
  inflightPromptIds: string[];
  /** Monotonic prompt count; an echo of this client's own optimistic prompt counts once. */
  promptSeq: number;
  /** Empty until the agent reports modes; the picker then falls back to the built-in four. */
  availableModes: Array<{
    id: string;
    name: string;
    description?: string | null;
  }>;
  currentModeId: string | null;
  availableCommands: AvailableCommand[];
  /** Adapter rejected `session/set_mode` (commonly bypassPermissions without `ALLOW_BYPASS`). */
  modeSwitchFailed: { modeId: string; reason: string; at: string } | null;
  workerStopped: boolean;
  workerRestarting: boolean;
  /** The worker was reaped for inactivity; the next prompt POST wakes it without a reconnect. */
  workerIdleStopped: boolean;
  /** Auto-resume gave up re-delivering the interrupted prompt and parked the session. */
  rateLimitRetriesExhausted: boolean;
  /** Parked on a rate limit, cap not yet reached. The daemon sends a prompt into this park
   *  rather than queueing it (the reconciler holds the respawn, so a queue row would wait on a
   *  worker nothing starts), so the composer must treat a worker-down 503 here as re-queueable. */
  rateLimitParked: boolean;
  /** Prompts submitted while a turn was running; the head is dispatched on `Stopped`. */
  queuedPrompts: QueuedPrompt[];
  nextWakeupAt: string | null;
  nextWakeupReason: string | null;
  /** An armed `Monitor` has no fire time; cleared when the user takes over or the fired turn ends. */
  monitorArmed: boolean;
  /** A tool call started after `MonitorArmed`, so the next `Stopped` may clear the badge. */
  monitorWorkSeen: boolean;
  monitorDescription: string | null;
  cancelling: boolean;
  /** When the cancel watchdog will SIGTERM the worker. */
  cancelEscalatesAt: string | null;
  /** The adapter goes silent for minutes while compacting; keeps the stall watchdog from killing it. */
  compacting: boolean;
  /** Context was reset after a prompt; the user may opt in to a primer. */
  contextPrimerAvailable: { resetSeq: number; reason: string } | null;
  /** Prompts rejected because another `session/prompt` was in flight; rendered as Retry pills. */
  rejectedPrompts: RejectedPrompt[];
  /** The cancel watchdog fired and the wedged worker is restarting. */
  agentUnresponsive: boolean;
  lastAgentSwitch: {
    from: string;
    to: string;
    reason: string;
    at: string;
  } | null;
  configOptions: ConfigOptionDescriptor[];
  configOptionSwitchFailed: ConfigOptionSwitchFailure | null;
  /** A config option click awaiting the next snapshot; the picker keeps showing the confirmed value. */
  pendingConfigOption: { configId: string; value: string } | null;
  /** The adapter finished streaming but never sent `PromptResponse`; the runner is respawning. */
  agentOrphaned: boolean;
  /** Async sub-agents launched this session, oldest first. */
  backgroundAgents: BackgroundAgent[];
}

export type BackgroundAgentStatus = "running" | "stalled" | "completed" | "detached" | "error";

export interface BackgroundAgentTool {
  name: string;
  title?: string | null;
  /** Undefined while running. */
  ok?: boolean | null;
}

export interface BackgroundAgent {
  agentId: string;
  /** The parent `Task` call that launched this agent. */
  toolCallId: string;
  description: string;
  prompt: string;
  model: string;
  status: BackgroundAgentStatus;
  startedAt: string;
  endedAt: string | null;
  toolCount: number;
  tools: BackgroundAgentTool[];
  lastTool: string | null;
  lastText: string | null;
  result: string | null;
  warning: string | null;
}

export interface RejectedPrompt {
  id: string;
  text: string;
  reason: string;
  rejectedAt: string;
}

export interface QueuedPrompt {
  id: string;
  text: string;
  queuedAt: string;
  /** Server-side after enqueue; local rows keep base64 only for the thumbnail and are never persisted with it. */
  attachments?: PromptAttachmentInput[];
  /** Unconfirmed optimistic row, kept by a hydrate that races the enqueue POST. */
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
  tool?: ToolCall;
  /** Rendered as `DiffCommentsUserCard`; `text` holds the markdown fallback. */
  diffComments?: {
    intro: string;
    outro: string;
    isMultiRepo: boolean;
    comments: DiffComment[];
  };
  attachments?: AcpAttachment[];
  output?: ToolOutputBlock[];
  elicitationAnswers?: ElicitationAnswer[];
  asyncSubagent?: boolean;
  at: string; // ISO-8601
}

/** Wire mirror of the Rust `TranscriptRow` (src/acp/transcript.rs). */
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

export type TranscriptDelta =
  | { Append: TranscriptRow }
  | { Patch: { id: string; row: TranscriptRow } }
  | { Remove: string };

/** The web shows `notice` rows as banners driven by folded state, so it skips them here. */
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

/** Merge server rows by id; a sparse `tool_start` folded on a later replay page must not clobber a richer one. */
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
      // Keep the earliest raw_name across a retitling merge for subagent classification.
      if (prev.tool.raw_name) merged.raw_name = prev.tool.raw_name;
      out[idx] = { ...prev, tool: merged, text: merged.name, at: merged.started_at };
    } else {
      out[idx] = row;
    }
  }
  return out;
}

/** Replace (or append) the row with `row.id`, keeping the earliest `raw_name` on a `tool_start`. */
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
    rateLimitParked: false,
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

/** A prompt steered into the running turn must not run {@link applyNewTurnResets}. Reads `serverTurnActive`, since `turnActive` is also true for this client's own unechoed prompt. */
function isSteeredContinuation(state: AcpState): boolean {
  return state.serverTurnActive && !!state.promptCapabilities?.steering;
}

function applyNewTurnResets(next: AcpState): void {
  next.startupError = null;
  next.lastError = null;
  // The cancel phase is server-owned; only the escalation deadline is ours.
  next.cancelEscalatesAt = null;
  next.workerStopped = false;
  next.workerRestarting = false;
  next.workerIdleStopped = false;
  next.rateLimitRetriesExhausted = false;
  next.rateLimitParked = false;
  next.rejectedPrompts = [];
  next.agentUnresponsive = false;
  next.agentOrphaned = false;
  // A mid-wait user prompt is not the /loop wake; only clear once the wake time has passed.
  if (next.nextWakeupAt) {
    const wakeAt = new Date(next.nextWakeupAt).getTime();
    if (!Number.isNaN(wakeAt) && Date.now() >= wakeAt) {
      next.nextWakeupAt = null;
      next.nextWakeupReason = null;
    }
  }
  // A monitor never self-fires a prompt, so any prompt here is the user taking over.
  next.monitorArmed = false;
  next.monitorWorkSeen = false;
  next.monitorDescription = null;
  next.contextPrimerAvailable = null;
}

/** Pure reducer. Drops frames whose seq is not above `state.lastSeq` so replays are idempotent. */
export function applyEvent(state: AcpState, frame: AcpFrame): AcpState {
  if (frame.seq <= state.lastSeq) {
    return state;
  }
  const next = { ...state, lastSeq: frame.seq };
  const event = frame.event;
  if (typeof event === "string") {
    // The agent reports lifetime cost, so each context boundary snapshots a baseline.
    if (event === "ThinkingStarted") {
      // Agent-initiated work still opens a turn, mirroring `AcpState::apply_event`.
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
    // A tool call after arming means the monitor fired.
    if (next.monitorArmed) {
      next.monitorWorkSeen = true;
    }
    return next;
  }
  if ("UsageUpdated" in event) {
    // Subtract the boundary baseline from the lifetime cost the agent reports.
    const incoming = event.UsageUpdated.usage;
    // Latch the largest window seen: upstream claude-agent-acp #596 flickers between 200k and 1M.
    const size = Math.max(incoming.size, next.sessionUsage?.size ?? 0);
    // Every boundary nulls sessionUsage, so a null previous snapshot re-arms the reminder.
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
    // A model change moves the context window, so relearn it.
    const priorModel = next.configOptions.find((o) => o.category === "model")?.current_value;
    const nextModel = options.find((o) => o.category === "model")?.current_value;
    if (priorModel !== undefined && nextModel !== undefined && priorModel !== nextModel) {
      next.sessionUsage = null;
    }
    next.configOptions = options;
    // The snapshot is authoritative; rejections arrive via ConfigOptionSwitchFailed.
    next.pendingConfigOption = null;
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
    // Whether this `Stopped` ends the turn is the daemon's call (see closeTurn); the escalation deadline is ours.
    next.cancelEscalatesAt = null;
    closeTurn(next);
    if (next.monitorArmed && next.monitorWorkSeen) {
      next.monitorArmed = false;
      next.monitorWorkSeen = false;
      next.monitorDescription = null;
    }
    // Any stop other than the limit itself ends the rate-limit park.
    if (event.Stopped.reason !== "rate_limited" && event.Stopped.reason !== "rate_limit_exhausted_retries") {
      next.rateLimit = null;
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
      next.workerRestarting = true;
      next.workerStopped = false;
      next.agentUnresponsive = true;
      next.agentOrphaned = false;
    } else if (event.Stopped.reason === "prompt_orphaned") {
      next.workerRestarting = true;
      next.workerStopped = false;
      next.agentUnresponsive = false;
      next.agentOrphaned = true;
    } else if (event.Stopped.reason === "idle_auto_stop") {
      next.workerIdleStopped = true;
      next.workerStopped = false;
      next.workerRestarting = false;
    } else if (event.Stopped.reason === "rate_limit_exhausted_retries") {
      next.rateLimitRetriesExhausted = true;
    } else if (event.Stopped.reason === "rate_limited") {
      next.rateLimitParked = true;
    }
    return next;
  }
  if ("IncompatibleAgent" in event) {
    next.incompatibleAgent = event.IncompatibleAgent.detail;
    next.agentUnresponsive = false;
    return next;
  }
  if ("AgentStartupError" in event) {
    next.startupError = event.AgentStartupError.message;
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
    // Opening the turn here keeps `turnActive` from flickering between settling the id and the `reduced_state` frame.
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
    next.promptSeq += 1;
    next.serverTurnActive = true;
    next.turnActive = true;
    if (!isSteeredContinuation(state)) {
      applyNewTurnResets(next);
    }
    return next;
  }
  if ("AcpSessionAssigned" in event) {
    next.startupError = null;
    next.lastError = null;
    next.incompatibleAgent = null;
    next.workerStopped = false;
    next.workerRestarting = false;
    next.workerIdleStopped = false;
    next.agentUnresponsive = false;
    next.agentOrphaned = false;
    next.rateLimit = null;
    next.rateLimitParked = false;
    return next;
  }
  if ("RateLimitAutoResumed" in event) {
    // Only the cap park ends here. The armed park outlives an auto-resume attempt: the daemon's
    // `rate_limit_park` does not treat this event as superseding it, so a failed respawn leaves
    // the session parked and a prompt still has to wake it.
    next.rateLimitRetriesExhausted = false;
    return next;
  }
  if ("SessionContextReset" in event) {
    next.sessionUsage = null;
    next.usageBaseline = null;
    // A session/load failure before any prompt is expected; no primer to offer.
    if (state.promptSeq <= 0) {
      return next;
    }
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
    next.monitorWorkSeen = false;
    next.monitorDescription = event.MonitorArmed.description ?? null;
    return next;
  }
  if ("CancelRequested" in event) {
    // The escalation deadline is not modelled server-side.
    next.cancelEscalatesAt = event.CancelRequested.escalates_at;
    return next;
  }
  if ("AgentSwitched" in event) {
    const { from, to, reason } = event.AgentSwitched;
    const now = new Date().toISOString();
    // What the new backend re-advertises is dropped server-side; client cost bookkeeping and banners stay here.
    next.sessionUsage = null;
    next.usageBaseline = null;
    next.startupError = null;
    next.lastAgentSwitch = { from, to, reason, at: now };
    // The switch emits Stopped { user_stopped } first; keep its banner hidden through the handshake.
    next.workerStopped = false;
    next.workerRestarting = false;
    next.agentUnresponsive = false;
    next.rateLimitRetriesExhausted = false;
    next.rateLimitParked = false;
    next.configOptions = [];
    next.configOptionSwitchFailed = null;
    next.pendingConfigOption = null;
    return next;
  }
  if ("PromptRejected" in event) {
    const entry: RejectedPrompt = {
      id: `rejected-${frame.seq}`,
      text: event.PromptRejected.text,
      reason: event.PromptRejected.reason,
      rejectedAt: new Date().toISOString(),
    };
    const REJECTED_PROMPTS_CAP = 5;
    next.rejectedPrompts = [...next.rejectedPrompts, entry].slice(-REJECTED_PROMPTS_CAP);
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
    // Idempotent on replay.
    const i = next.backgroundAgents.findIndex((a) => a.agentId === e.agent_id);
    next.backgroundAgents =
      i >= 0 ? next.backgroundAgents.map((a, idx) => (idx === i ? record : a)) : [...next.backgroundAgents, record];
    return next;
  }
  if ("BackgroundAgentProgress" in event) {
    const e = event.BackgroundAgentProgress;
    next.backgroundAgents = next.backgroundAgents.map((a) => {
      if (a.agentId !== e.agent_id) return a;
      // A terminal record never reopens to running. Also guarded on endedAt:
      // a terminal Stalled record (the tailer's own abort timeout) carries
      // endedAt too, and the status check alone would let a late Progress
      // reopen it, contradicting hasActiveBackgroundAgent's endedAt-keyed read.
      if (a.endedAt || a.status === "completed" || a.status === "detached" || a.status === "error") return a;
      return {
        ...a,
        status: e.status,
        toolCount: e.tool_count,
        tools: e.tools && e.tools.length > 0 ? e.tools : a.tools,
        // A Progress never ends an agent; only the terminal
        // BackgroundAgentCompleted sets endedAt (#4001).
        endedAt: null,
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
  // Remaining events carry no client control state.
  return next;
}

/** Fold a self-contained run of frames from an empty state, e.g. an older history page. */
export function reduceFrames(frames: AcpFrame[]): AcpState {
  return frames.reduce(applyEvent, emptyAcpState());
}

/** Adopt the daemon's folded control state. Leaves `lastSeq` to the raw frames that dedupe replays. */
export function applyReducedState(state: AcpState, reduced: ReducedState, unchanged: string[] = []): AcpState {
  // Omitted cold fields arrive as empty defaults; adopting them would blank the pickers.
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
    // A false frame cannot suppress a prompt whose POST is still unacknowledged.
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

/** Merge a duplicate `ToolCallStarted`: a sparse permission start must not clobber a real one. */
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
    raw_name: incoming.raw_name && incoming.raw_name.length > 0 ? incoming.raw_name : prev.raw_name,
    kind: incoming.kind && incoming.kind !== "other" ? incoming.kind : prev.kind,
    args_preview: incoming.args_preview.trim().length > 0 ? incoming.args_preview : prev.args_preview,
    started_at: startedAt,
    diffs: incoming.diffs && incoming.diffs.length > 0 ? incoming.diffs : prev.diffs,
    parent_tool_call_id: incoming.parent_tool_call_id ?? prev.parent_tool_call_id,
    memory_recall: incoming.memory_recall ?? prev.memory_recall,
  };
}

/** Prepend an older page, merging a `tool_start` split across the page seam into the tail's synthesized placeholder. */
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
      // The placeholder carried the completion time, which would zero the duration.
      if (row.tool.started_at) merged.started_at = row.tool.started_at;
      tail = tail.slice();
      tail[idx] = { ...existing, tool: merged, text: merged.name, at: merged.started_at };
    }
  }
  return prepended.concat(tail);
}

/** Optimistic `elicitation_answered` row, dropped when the server's same-id row lands. */
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

/** Mid-turn steering can complete several prompts with one event, so prompt counters cannot derive this. */
export function deriveTurnActive(state: Pick<AcpState, "serverTurnActive" | "inflightPromptIds">): boolean {
  return state.serverTurnActive || state.inflightPromptIds.length > 0;
}

/** Outstanding background sub-agent (async Task), mirroring
 *  `AcpState::has_active_background_agent`: keyed on `endedAt` rather than
 *  status, since a stalled agent's own terminal completion counts as done. */
export function hasActiveBackgroundAgent(state: Pick<AcpState, "backgroundAgents">): boolean {
  return state.backgroundAgents.some((a) => a.endedAt === null);
}

/** Display-only busy signal for the runtime spinner. Distinct from `turnActive`,
 *  which tracks only the main turn because send-vs-queue composer gating reads it. */
export function isVisiblyBusy(state: Pick<AcpState, "turnActive" | "backgroundAgents">): boolean {
  return state.turnActive || hasActiveBackgroundAgent(state);
}

/** Mirror the daemon's turn-close edges; `GET /acp/replay` serves raw events without `reduced_state`. */
function closeTurn(next: AcpState): void {
  next.serverTurnActive = false;
  next.turnActive = deriveTurnActive(next);
}

/** Whether to show the compaction reminder. Needs an advertised `compact` command and a reported window; `used > size` counts as due. */
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

/** Backfill turn state and newer fields on a localStorage entry persisted by an older client. */
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
  const promptSeq =
    typeof state.promptSeq === "number" && Number.isFinite(state.promptSeq)
      ? Math.max(0, Math.floor(state.promptSeq))
      : (state.activity ?? []).filter((r) => r.kind === "user_prompt").length;
  const rejectedPrompts = Array.isArray(state.rejectedPrompts) ? state.rejectedPrompts : [];
  const agentUnresponsive = typeof state.agentUnresponsive === "boolean" ? state.agentUnresponsive : false;
  const agentOrphaned = typeof state.agentOrphaned === "boolean" ? state.agentOrphaned : false;
  const usageBaseline = state.usageBaseline === undefined ? null : state.usageBaseline;
  const configOptions = Array.isArray(state.configOptions) ? state.configOptions : [];
  const configOptionSwitchFailed = state.configOptionSwitchFailed === undefined ? null : state.configOptionSwitchFailed;
  const pendingConfigOption = state.pendingConfigOption === undefined ? null : state.pendingConfigOption;
  const compactionReminderDismissed =
    state.compactionReminderDismissed === undefined ? null : state.compactionReminderDismissed;
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

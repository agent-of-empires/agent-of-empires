// Cockpit wire types. Mirror the shapes emitted by the Rust
// `CockpitBroadcastFrame` serializer + the `Event` enum in
// `src/cockpit/state.rs`. These are intentionally permissive: the Rust
// side can add new variants without breaking the UI as long as the
// component renders unknown frames gracefully.

import type { DiffComment } from "../components/diff/comments/types";

export type ApprovalDecision =
  | "Allow"
  | "AllowAlways"
  | "Deny"
  | "Cancelled";

export type SessionMode =
  | "Default"
  | "Plan"
  | "AcceptEdits"
  | "BypassPermissions";

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
  /** ACP ToolKind lowercased: read | edit | delete | move | search |
   *  execute | think | fetch | switch_mode | other. Drives the per-tool
   *  renderer in CockpitView. */
  kind: string;
  args_preview: string;
  started_at: string; // ISO-8601 from chrono
  /** When the agent launches a sub-agent (Claude's Task tool), the
   *  adapter rides `_meta.claudeCode.parentToolUseId` along on the
   *  child tool calls. Threaded through here so the cockpit can group
   *  sub-tools under their parent Task. Undefined for top-level
   *  calls. See #1041. */
  parent_tool_call_id?: string;
  /** Populated when claude-agent-acp v0.37.0+ routes a session-start
   *  memory recall through the tool channel (upstream #703). The
   *  cockpit renders a dedicated MemoryRecallCard instead of treating
   *  it as a generic read. `recall` mode carries the list of file
   *  paths the SDK loaded into the agent's context; `synthesize`
   *  mode carries the synthesised memory text. */
  memory_recall?: MemoryRecall | null;
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

export interface RateLimitInfo {
  status: string;
  resets_at: string;
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
 *  ACP's `SessionConfigOptionCategory`. The cockpit UI uses this to
 *  pick the right widget per category (model dropdown, effort
 *  segmented control). The Rust `Other(String)` arm is
 *  `#[serde(untagged)]`, so an unknown category arrives on the wire as
 *  a bare string, not a `{ Other: string }` object. Modeling it as a
 *  catch-all string keeps the broadcast frame forward-compatible while
 *  preserving autocomplete on the known literals. See #1403, #1562. */
export type ConfigOptionCategory =
  | "mode"
  | "model"
  | "thought_level"
  | (string & {});

/** One choice in a `Select`-kind ConfigOptionDescriptor. */
export interface ConfigOptionChoice {
  value: string;
  name: string;
  description?: string | null;
}

/** Cockpit's view of a single ACP `SessionConfigOption`. Each
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
 *  `CockpitState.configOptionSwitchFailed` so the UI can render a
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

/** Mirror of `StartupErrorDetail` in src/cockpit/state.rs. Serde's
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
    }
  | {
      kind: "missing_agent_info";
      expected_package: string;
      install_command: string;
    }
  | {
      kind: "mismatched_agent_name";
      expected: string;
      received: string;
      install_command: string;
    }
  | {
      kind: "unparseable_agent_version";
      package_name: string;
      raw_version: string;
      required: string;
      install_command: string;
    }
  | {
      kind: "unsupported_protocol_version";
      expected: string;
      received: string;
    };

// One variant per Event::* in src/cockpit/state.rs. All variants carry
// a discriminant key matching the serde representation: serde defaults
// to externally-tagged JSON for an enum, e.g.
// { "ApprovalRequested": { "approval": ... } }.
export type CockpitEvent =
  | { PlanUpdated: { plan: Plan } }
  | { TodoListUpdated: { todos: Array<{ id: string; text: string; completed: boolean }> } }
  | { ToolCallStarted: { tool_call: ToolCall } }
  | {
      ToolCallCompleted: {
        tool_call_id: string;
        is_error: boolean;
        /** Final textual content extracted from
         *  ACP `ToolCallUpdate.fields.content`. Empty when the agent
         *  emitted no content blocks on completion. */
        content: string;
        /** Server-side ISO-8601 wall clock at which the completion
         *  was minted. Used to stamp the activity row's `at` so the
         *  duration label survives page reload; without it, the
         *  reducer would assign `new Date()` at replay time and the
         *  measured duration would count from "now". Optional for
         *  backward compatibility with events persisted before this
         *  field landed. */
        completed_at?: string;
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
         *  status transitioned to InProgress. See acp_client.rs;
         *  reused so the duration label measures real tool runtime
         *  rather than adapter scheduling time. Null for non-status
         *  updates. */
        started_at?: string | null;
      };
    }
  | { ApprovalRequested: { approval: Approval } }
  | { ApprovalResolved: { nonce: string; decision: ApprovalDecision } }
  | "SessionCleared"
  | "ConversationCompacted"
  | { DiffEmitted: { diff: DiffPreview } }
  | "ThinkingStarted"
  | "ThinkingEnded"
  | { RateLimit: { info: RateLimitInfo } }
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
  | { AgentMessageChunk: { text: string } }
  | { CancelRequested: { escalates_at: string } }
  | { Stopped: { reason: string } }
  | { AgentStartupError: { message: string } }
  | { IncompatibleAgent: { detail: IncompatibleAgentDetail } }
  | { UserPromptSent: { text: string; attachments?: PromptAttachmentRefWire[] } }
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
      };
    }
  | { AcpSessionAssigned: { acp_session_id: string } }
  | { SessionContextReset: { reason: string } }
  | { WakeupScheduled: { at: string; reason: string | null } }
  | { PromptRejected: { reason: string; text: string } }
  | { AgentSwitched: { from: string; to: string; reason: string } };

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
export interface CockpitAttachment {
  id: string;
  kind: PromptAttachmentKind;
  mimeType: string;
  name?: string;
  size: number;
  url: string;
}

export interface CockpitFrame {
  session_id: string;
  seq: number;
  event: CockpitEvent;
}

export interface CockpitState {
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
  recentDiffs: DiffPreview[];
  thinking: boolean;
  rateLimit: RateLimitInfo | null;
  /** Latest agent-reported context-window usage. Null until the agent
   *  emits its first ACP `UsageUpdate`. */
  sessionUsage: SessionUsage | null;
  /** Cumulative cost snapshot captured at the most recent context
   *  boundary (`/clear`, `/compact`). The ACP agent keeps reporting
   *  session-lifetime cumulative cost via `UsageUpdate`, but the user
   *  expects the composer footer to read "since the most recent
   *  clear/compact." The `UsageUpdated` reducer arm subtracts this
   *  baseline from the incoming cumulative before storing it on
   *  `sessionUsage.cost`. Reset to null on `AgentSwitched` and
   *  `SessionContextReset` (new backend or new ACP session restarts
   *  the agent-side cumulative at zero). Re-derived on hard reload via
   *  the full event-store replay, since boundary events are applied in
   *  seq order. See #1354. */
  usageBaseline: { cost: number } | null;
  /** Most recent assistant message chunks accumulated as a single
   *  text body. Cleared each time a new prompt is sent. */
  assistantMessage: string;
  /** Activity rows (tool starts + completions + agent messages),
   *  oldest first. Bounded for memory. */
  activity: ActivityRow[];
  /** Last seen seq, for reconnect requests. Frames whose `seq` is
   *  not strictly greater than this are dropped by the reducer so
   *  reconnect-replay can deliver the same frames again without
   *  double-applying them to state. */
  lastSeq: number;
  /** True if the most recent broadcast told us we lagged. Cleared
   *  the next time the client successfully resyncs via the snapshot
   *  endpoint. */
  lagged: boolean;
  /** Latest agent startup failure message, if any. Cleared when a new
   *  prompt is sent or the worker successfully connects. */
  startupError: string | null;
  /** Structured detail from the per-adapter compatibility check (see
   *  `src/cockpit/agent_compat.rs`). When set, the cockpit UI replaces
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
   *  Derived from `pendingUserPromptSeq > lastStoppedSeq`; never
   *  written directly. Keeping it on the state shape (instead of
   *  exporting a selector) lets all the existing `state.turnActive`
   *  reads stay unchanged. The counter pair is the source of truth so
   *  a late `Stopped` from a prior turn can't clobber a fresh
   *  follow-up that's already incremented `pendingUserPromptSeq`.
   *  See #1170. */
  turnActive: boolean;
  /** Monotonic count of user prompts the client has dispatched (either
   *  via the optimistic `user_prompt` action or via a server-confirmed
   *  `UserPromptSent` echo that didn't match an outstanding optimistic
   *  row). Source of truth for `turnActive`; never decremented. */
  pendingUserPromptSeq: number;
  /** Snapshot of `pendingUserPromptSeq` at the moment the most recent
   *  `Stopped` (or `AgentStartupError`) arrived. `turnActive` derives
   *  to false only when no further prompt has bumped
   *  `pendingUserPromptSeq` past this snapshot. */
  lastStoppedSeq: number;
  /** Real ACP-advertised modes from the agent's NewSessionResponse,
   *  plus the agent's currently-active mode id. Empty until the
   *  agent reports them; the picker falls back to the hard-coded
   *  four-mode taxonomy in that case. */
  availableModes: Array<{ id: string; name: string; description?: string | null }>;
  currentModeId: string | null;
  /** Slash commands the agent advertised in its most recent
   *  `AvailableCommandsUpdate`. Empty until the agent emits one; the
   *  composer's `/` picker reads from here. */
  availableCommands: AvailableCommand[];
  /** Streaming output buffer keyed by tool_call_id. Populated by
   *  ToolCallContent frames while the call is still running, drained
   *  on ToolCallCompleted (used as a fallback when the completion
   *  carries no content of its own). */
  toolOutputs: Record<string, string>;
  /** True iff the current turn has produced at least one piece of
   *  visible output (assistant chunk, tool call, thinking signal).
   *  Reset to false on every UserPromptSent. Used by the Stopped
   *  handler to detect "no-op turn" without walking the full
   *  activity array. */
  turnHasOutput: boolean;
  /** Latest cockpit-side `session/set_mode` rejection from the adapter.
   *  Populated by the `ModeSwitchFailed` event so the UI can render a
   *  non-blocking notice ("Yolo / bypassPermissions requested but the
   *  adapter declined; session is in default mode"). Most common cause:
   *  claude-agent-acp gates bypassPermissions on the `ALLOW_BYPASS` env
   *  var. Cleared by the user dismissing the notice or by a successful
   *  `CurrentModeChanged`. See #1233. */
  modeSwitchFailed: { modeId: string; reason: string; at: string } | null;
  /** Set true when the daemon publishes `Stopped { reason: "user_stopped" }`,
   *  meaning `aoe cockpit stop|kill` (or an equivalent external
   *  teardown) terminated the runner. The composer disables itself and
   *  shows a reconnect banner; cleared on the next UserPromptSent or
   *  AcpSessionAssigned (a fresh worker is online). */
  workerStopped: boolean;
  /** Set true when the daemon publishes `Stopped { reason: "restart_pending" }`,
   *  meaning `aoe cockpit restart` ran and the reconciler will respawn
   *  the worker on its next 2s tick with the cached `acp_session_id`
   *  (transcript continuity). The composer disables itself and a
   *  transient "Restarting…" banner appears without a reconnect button;
   *  cleared on AcpSessionAssigned or UserPromptSent. */
  workerRestarting: boolean;
  /** Set true when the daemon publishes `Stopped { reason: "idle_auto_stop" }`,
   *  meaning the reconciler reaped the worker for inactivity
   *  (`cockpit.auto_stop_idle_secs`) and marked the session dormant. Unlike
   *  `workerStopped`, this is recoverable without any explicit reconnect:
   *  the next prompt POST wakes it (the server's `touch_and_wake_if_sunk`
   *  clears dormancy, the reconciler respawns, and `send_prompt`'s
   *  `wait_for_worker` holds the request until the fresh worker is ready).
   *  `sendPrompt` and the drain effect read this so a dormant worker does
   *  NOT park prompts in the local queue forever; instead the POST itself
   *  is the wake path. Cleared on AcpSessionAssigned or UserPromptSent. */
  workerIdleStopped: boolean;
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
   *  the cockpit banner next to the countdown. */
  nextWakeupReason: string | null;
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
   *  `/cockpit/switch-agent`. Drives a transcript divider ("Switched
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
    | "empty_output"
    | "context_reset"
    | "session_cleared"
    | "compacted";
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
  attachments?: CockpitAttachment[];
  at: string; // ISO-8601
}

/** Module-level mirror of `cockpit.replay_events`. Set by the
 *  `useCockpit` hook from `useCockpitPrefs` so the reducer (which
 *  can't read React context) sees the user's chosen retention cap.
 *  0 means unlimited. Default 0 matches `cockpit.replay_events`'
 *  default after #1065 made server-side retention unlimited; without
 *  this mirror, a frontend-only 200-row cap clipped the rendered
 *  transcript regardless of what the user set on the server side.
 *  See #1111. */
let activityLimit = 0;

/** Set the activity buffer cap. Called by `useCockpit` whenever the
 *  resolved prefs change so the reducer's `pushActivity` honours
 *  the current setting. Visible for tests that need to pin the cap. */
export function setActivityLimit(limit: number): void {
  activityLimit = Math.max(0, Math.floor(limit));
}

export function emptyCockpitState(): CockpitState {
  return {
    agent: null,
    model: null,
    mode: "Default",
    promptCapabilities: null,
    plan: null,
    inFlightTool: null,
    pendingApprovals: [],
    recentDiffs: [],
    thinking: false,
    rateLimit: null,
    sessionUsage: null,
    usageBaseline: null,
    assistantMessage: "",
    activity: [],
    lastSeq: 0,
    lagged: false,
    startupError: null,
    incompatibleAgent: null,
    lastError: null,
    turnActive: false,
    pendingUserPromptSeq: 0,
    lastStoppedSeq: 0,
    availableModes: [],
    currentModeId: null,
    availableCommands: [],
    toolOutputs: {},
    turnHasOutput: false,
    workerStopped: false,
    workerRestarting: false,
    workerIdleStopped: false,
    queuedPrompts: [],
    nextWakeupAt: null,
    nextWakeupReason: null,
    cancelling: false,
    cancelEscalatesAt: null,
    contextPrimerAvailable: null,
    rejectedPrompts: [],
    agentUnresponsive: false,
    agentOrphaned: false,
    modeSwitchFailed: null,
    lastAgentSwitch: null,
    configOptions: [],
    configOptionSwitchFailed: null,
    pendingConfigOption: null,
  };
}

/** Per-turn state resets shared by every "a new user turn started"
 *  event (a plain `UserPromptSent` and a `UserDiffCommentsPrompt`).
 *  Mutates `next` in place; the caller has already appended the
 *  activity row and bumped `pendingUserPromptSeq`. */
function applyNewTurnResets(next: CockpitState): void {
  next.assistantMessage = "";
  next.startupError = null;
  next.lastError = null;
  next.turnActive = isTurnActive(next);
  // A fresh turn supersedes any stale "Stopping..." state from a prior
  // turn's cancel. See #1727.
  next.cancelling = false;
  next.cancelEscalatesAt = null;
  // New turn; reset the no-output detector so Stopped fires the
  // empty-output notice if the agent produces nothing.
  next.turnHasOutput = false;
  // A fresh prompt means the worker is alive again; clear the
  // user_stopped banner without waiting for AcpSessionAssigned.
  next.workerStopped = false;
  next.workerRestarting = false;
  // A prompt also wakes an idle-dormant worker (the POST cleared
  // dormancy server-side); drop the marker so the drain effect stops
  // treating the worker as wakeable-but-down.
  next.workerIdleStopped = false;
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
  // Any pending context-primer offer is consumed once the user submits
  // a new prompt; the recovery affordance is one-shot.
  next.contextPrimerAvailable = null;
}

/** Pure reducer. Returns a new state; never mutates the input.
 *  Drops frames whose seq is not strictly greater than `state.lastSeq`
 *  so reconnect/replay can re-deliver buffered frames without
 *  double-applying them (duplicate tool cards, doubled message
 *  chunks, etc.). */
export function applyEvent(
  state: CockpitState,
  frame: CockpitFrame,
): CockpitState {
  if (frame.seq <= state.lastSeq) {
    return state;
  }
  const next = { ...state, lastSeq: frame.seq };
  const event = frame.event;
  if (typeof event === "string") {
    if (event === "ThinkingStarted") {
      next.thinking = true;
      next.turnHasOutput = true;
    } else if (event === "ThinkingEnded") {
      next.thinking = false;
    } else if (event === "ConversationCompacted") {
      // /compact replaced the model's context with a summary. The
      // model still has continuity through the summary so no primer
      // affordance is appropriate; just drop the now-stale usage
      // snapshot and append a divider row. The renderer maps the
      // `compacted` kind to a "Conversation compacted" divider that
      // makes the boundary visible without nudging the user toward
      // pre-filling duplicate context. See #1109.
      // Capture the agent's cumulative cost snapshot as the new
      // baseline so the next UsageUpdate reports cost-since-compact
      // instead of session-lifetime cumulative. See #1354.
      const compactPriorUsage = state.sessionUsage?.cost?.amount ?? 0;
      const compactPriorBaseline = state.usageBaseline?.cost ?? 0;
      const compactCumulative = compactPriorUsage + compactPriorBaseline;
      next.usageBaseline = { cost: compactCumulative };
      next.sessionUsage = null;
      next.activity = [
        ...next.activity,
        {
          id: `compacted-${frame.seq}`,
          kind: "compacted",
          text: "Conversation compacted; earlier turns above are summarised in the model's context.",
          at: new Date().toISOString(),
        },
      ];
    } else if (event === "SessionCleared") {
      // /clear wiped the model's memory. Append a divider row so the
      // UI can fold pre-clear turns behind a disclosure (#1101), then
      // drop only the per-turn / in-flight state that the cleared
      // context invalidates: the active plan, the legacy mode enum,
      // pending approvals, and the session usage snapshot.
      //
      // We deliberately preserve availableCommands, availableModes,
      // and currentModeId. claude-agent-sdk caches the supported
      // command surface at Query init and does not recreate the
      // Query on /clear, so the cached list stays authoritative for
      // the lifetime of the cockpit's underlying agent process. The
      // prior over-clear (#1101 A.1) was based on an assumption that
      // doesn't hold for this SDK; emptying availableCommands made
      // the slash palette stay empty forever after the first /clear
      // because no AvailableCommandsUpdated event arrives to refill
      // it (tracked upstream at
      // agentclientprotocol/claude-agent-acp#657). See #1128.
      next.activity = [
        ...next.activity,
        {
          id: `cleared-${frame.seq}`,
          kind: "session_cleared",
          text: "Conversation cleared, the model no longer remembers earlier turns.",
          at: new Date().toISOString(),
        },
      ];
      next.plan = null;
      next.mode = "Default";
      next.pendingApprovals = [];
      // Capture the agent's cumulative cost snapshot as the new
      // baseline so the next UsageUpdate reports cost-since-clear
      // instead of session-lifetime cumulative. `sessionUsage.cost`
      // already stores the delta since the previous baseline, so the
      // new baseline is the sum of both to track the true cumulative.
      // See #1354.
      const clearPriorUsage = state.sessionUsage?.cost?.amount ?? 0;
      const clearPriorBaseline = state.usageBaseline?.cost ?? 0;
      const clearCumulative = clearPriorUsage + clearPriorBaseline;
      next.usageBaseline = { cost: clearCumulative };
      next.sessionUsage = null;
    }
    return next;
  }
  if ("PlanUpdated" in event) {
    next.plan = event.PlanUpdated.plan;
    return next;
  }
  if ("ToolCallStarted" in event) {
    const tc = event.ToolCallStarted.tool_call;
    next.inFlightTool = tc;
    // The reasoning block produced output (a tool call), so the agent is
    // no longer thinking. The adapter often skips ThinkingEnded when it
    // transitions into tool calls, so clear it here. See #1213.
    next.thinking = false;
    // Skip duplicate tool_start rows. SQLite stores accumulated from
    // pre-fix runs (where post-load history-replay leaked through) can
    // contain the same tool_call_id twice; rendering both makes
    // assistant-ui's tapResources throw "Duplicate key" and crash the
    // panel. Patch the existing row in place instead.
    const existing = next.activity.findIndex(
      (r) => r.kind === "tool_start" && r.toolCallId === tc.id,
    );
    if (existing >= 0) {
      const prev = next.activity[existing];
      if (prev) {
        const copy = next.activity.slice();
        copy[existing] = { ...prev, tool: tc, text: tc.name };
        next.activity = copy;
      }
      return next;
    }
    next.activity = pushActivity(next.activity, {
      id: `start-${tc.id}`,
      kind: "tool_start",
      text: tc.name,
      toolCallId: tc.id,
      tool: tc,
      at: tc.started_at,
    });
    next.turnHasOutput = true;
    return next;
  }
  if ("ToolCallCompleted" in event) {
    const { tool_call_id, is_error, content, completed_at } =
      event.ToolCallCompleted;
    if (next.inFlightTool && next.inFlightTool.id === tool_call_id) {
      next.inFlightTool = null;
    }
    // Prefer content shipped with the completion event itself; fall
    // back to whatever streamed earlier via ToolCallContent. Only use
    // the status word when neither carried text.
    const buffered = next.toolOutputs[tool_call_id] ?? "";
    const text =
      content && content.length > 0
        ? content
        : buffered.length > 0
          ? buffered
          : is_error
            ? "tool failed"
            : "completed";
    if (buffered) {
      const { [tool_call_id]: _drop, ...rest } = next.toolOutputs;
      void _drop;
      next.toolOutputs = rest;
    }
    // Use the server-side completion timestamp when present so the
    // duration label survives page reload. Events persisted before
    // `completed_at` landed fall back to "now" (same bug as before for
    // those specific rows only).
    next.activity = pushActivity(next.activity, {
      id: `done-${tool_call_id}`,
      kind: is_error ? "tool_error" : "tool_complete",
      text,
      toolCallId: tool_call_id,
      at: completed_at ?? new Date().toISOString(),
    });
    return next;
  }
  if ("ToolCallContent" in event) {
    const { tool_call_id, content } = event.ToolCallContent;
    next.toolOutputs = { ...next.toolOutputs, [tool_call_id]: content };
    return next;
  }
  if ("ToolCallUpdated" in event) {
    const { tool_call_id, title, args_preview, started_at } =
      event.ToolCallUpdated;
    if (next.inFlightTool && next.inFlightTool.id === tool_call_id) {
      next.inFlightTool = {
        ...next.inFlightTool,
        name: title ?? next.inFlightTool.name,
        args_preview: args_preview ?? next.inFlightTool.args_preview,
        started_at: started_at ?? next.inFlightTool.started_at,
      };
    }
    // Walk activity backwards to find the matching tool_start row and
    // patch its `tool` payload in place. AssistantBuilder reads from
    // here at render time, so updating the row is enough to refresh
    // the per-tool card.
    let patched = false;
    const updated = next.activity.map((row) => {
      if (
        !patched &&
        row.kind === "tool_start" &&
        row.toolCallId === tool_call_id &&
        row.tool
      ) {
        patched = true;
        return {
          ...row,
          text: title ?? row.text,
          tool: {
            ...row.tool,
            name: title ?? row.tool.name,
            args_preview: args_preview ?? row.tool.args_preview,
            started_at: started_at ?? row.tool.started_at,
          },
        };
      }
      return row;
    });
    if (patched) next.activity = updated;
    return next;
  }
  if ("ApprovalRequested" in event) {
    const a = event.ApprovalRequested.approval;
    next.pendingApprovals = [...next.pendingApprovals, a];
    return next;
  }
  if ("ApprovalResolved" in event) {
    const { nonce } = event.ApprovalResolved;
    next.pendingApprovals = next.pendingApprovals.filter(
      (a) => a.nonce !== nonce,
    );
    return next;
  }
  if ("DiffEmitted" in event) {
    next.recentDiffs = [...next.recentDiffs, event.DiffEmitted.diff].slice(-16);
    return next;
  }
  if ("RateLimit" in event) {
    next.rateLimit = event.RateLimit.info;
    return next;
  }
  if ("UsageUpdated" in event) {
    // claude-agent-acp keeps reporting session-lifetime cumulative
    // cost via UsageUpdate; `/clear` and `/compact` don't rotate the
    // ACP session id so the agent's cumulative carries pre-boundary
    // spend. Subtract the baseline captured at the most recent
    // SessionCleared / ConversationCompacted so the composer footer
    // reads "since the most recent boundary." `used` already reflects
    // the post-boundary context size from the agent's side and stays
    // raw; only `cost` is rebased. clamp to zero defensively in case
    // an upstream restart ever reports a smaller cumulative. See #1354.
    const incoming = event.UsageUpdated.usage;
    if (next.usageBaseline && incoming.cost) {
      const rebasedAmount = Math.max(0, incoming.cost.amount - next.usageBaseline.cost);
      const rebasedCost = { amount: rebasedAmount, currency: incoming.cost.currency };
      next.sessionUsage = { used: incoming.used, size: incoming.size, cost: rebasedCost };
    } else {
      next.sessionUsage = incoming;
    }
    return next;
  }
  if ("ModeChanged" in event) {
    next.mode = event.ModeChanged.mode;
    return next;
  }
  if ("ModesAvailable" in event) {
    next.availableModes = event.ModesAvailable.modes.map((m) => ({
      id: m.id,
      name: m.name,
      description: m.description ?? null,
    }));
    next.currentModeId = event.ModesAvailable.current_mode_id;
    return next;
  }
  if ("CurrentModeChanged" in event) {
    next.currentModeId = event.CurrentModeChanged.current_mode_id;
    // Mode actually switched, so any prior failure notice is stale.
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
  if ("AvailableCommandsUpdated" in event) {
    next.availableCommands = event.AvailableCommandsUpdated.commands;
    return next;
  }
  if ("ConfigOptionsUpdated" in event) {
    const options = event.ConfigOptionsUpdated.options;
    next.configOptions = options;
    // The snapshot is authoritative, so any in-flight pending click
    // resolves here regardless of whether the adapter applied the
    // exact requested value. A rejected change comes through
    // `ConfigOptionSwitchFailed` and clears pending on that path
    // instead.
    next.pendingConfigOption = null;
    // Auto-dismiss a stale switch-failed notice when this snapshot
    // confirms the originally-requested value: user retried and won,
    // or the adapter applied asynchronously after the rejection.
    if (next.configOptionSwitchFailed) {
      const failure = next.configOptionSwitchFailed;
      const confirmed = options.some(
        (opt) =>
          opt.id === failure.configId && opt.current_value === failure.value,
      );
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
  if ("AgentMessageChunk" in event) {
    next.assistantMessage = next.assistantMessage + event.AgentMessageChunk.text;
    // Visible assistant text means the agent is answering, not thinking.
    // A later reasoning block re-sets `thinking` via ThinkingStarted. See
    // #1213.
    next.thinking = false;
    next.activity = pushActivity(next.activity, {
      id: `msg-${frame.seq}`,
      kind: "message",
      text: event.AgentMessageChunk.text,
      at: new Date().toISOString(),
    });
    next.turnHasOutput = true;
    return next;
  }
  if ("Stopped" in event) {
    // Final marker; nothing to mutate, but reset the inflight tool just
    // in case the agent forgot to emit a completion.
    //
    // `turnActive` is derived from `pendingUserPromptSeq > lastStoppedSeq`;
    // we advance `lastStoppedSeq` by one (capped at `pendingUserPromptSeq`)
    // so this Stopped only retires ONE turn's worth of activity. If a
    // fresh user prompt landed client-side between the turn this Stopped
    // is closing and now, `pendingUserPromptSeq` was already bumped past
    // the cap and `turnActive` stays true. Without this, a late Stopped
    // would clobber the spinner mid follow-up and reorder the user's
    // optimistic message above any still-arriving prior-turn agent
    // chunks. See #1170.
    next.inFlightTool = null;
    // Belt-and-suspenders against a missed ThinkingEnded leaking the
    // thinking state into the next turn (same defensive shape as the
    // inFlightTool reset above). See #1213.
    next.thinking = false;
    sweepOpenToolCalls(next, frame.seq);
    // The turn ended (cleanly, cancelled, force-stopped, or escalated):
    // clear the "Stopping..." state regardless of reason. See #1727.
    next.cancelling = false;
    next.cancelEscalatesAt = null;
    next.lastStoppedSeq = Math.min(
      next.lastStoppedSeq + 1,
      next.pendingUserPromptSeq,
    );
    next.turnActive = isTurnActive(next);
    // The "user_stopped" / "restart_pending" reasons are published by
    // the supervisor's reap_user_stopped pass when it detects an
    // out-of-band CLI teardown. Surface a distinct UI state for each:
    //   - user_stopped: persistent "Stopped" banner with a Reconnect
    //     button; the daemon will NOT auto-respawn.
    //   - restart_pending: transient "Restarting…" banner without a
    //     reconnect affordance; the reconciler will respawn within ~2s
    //     and AcpSessionAssigned clears the flag.
    if (event.Stopped.reason === "user_stopped") {
      next.workerStopped = true;
      next.workerRestarting = false;
      // Any prior unresponsive escalation has been superseded by the
      // user explicitly stopping the worker; drop the stale banner
      // flag so a future `restart_pending` doesn't accidentally
      // render unresponsive copy. See #1196.
      next.agentUnresponsive = false;
      next.agentOrphaned = false;
    } else if (event.Stopped.reason === "restart_pending") {
      next.workerRestarting = true;
      next.workerStopped = false;
      next.agentUnresponsive = false;
      next.agentOrphaned = false;
    } else if (event.Stopped.reason === "agent_unresponsive") {
      // Cancel-escalation watchdog in the daemon fired: claude-agent-acp
      // ignored `session/cancel` for the grace window, the supervisor
      // is SIGTERMing the runner and respawning via `session/load` to
      // preserve transcript continuity. claude-agent-acp >=0.37.0
      // (upstream #694) returns StopReason::Cancelled natively when it
      // resolves the cancel; in that path the daemon surfaces
      // `cancelled` instead and this branch only fires when the adapter
      // does not respond at all (transport wedge, child hang). Reuse
      // `workerRestarting`'s composer-lockdown semantics; the
      // `agentUnresponsive` flag lets the banner render the specific
      // cause. Cleared on `AcpSessionAssigned` (respawn finished) or
      // `UserPromptSent`. See #1196.
      next.workerRestarting = true;
      next.workerStopped = false;
      next.agentUnresponsive = true;
      next.agentOrphaned = false;
    } else if (event.Stopped.reason === "prompt_orphaned") {
      // Silent-orphan watchdog in the daemon fired: the adapter
      // finished streaming the turn but never sent the JSON-RPC
      // `PromptResponse`, the supervisor is SIGTERMing the runner
      // and respawning via `session/load` (transcript preserved).
      // Distinct from `agent_unresponsive`: this is "adapter stopped
      // talking" vs. "adapter ignored cancel"; both reuse the
      // `workerRestarting` lockdown, but the banner copy differs so
      // users can tell which failure happened. See #1240.
      next.workerRestarting = true;
      next.workerStopped = false;
      next.agentUnresponsive = false;
      next.agentOrphaned = true;
    } else if (event.Stopped.reason === "idle_auto_stop") {
      // The reconciler reaped the worker for inactivity and marked the
      // session dormant (#1689). This is NOT a user stop: no reconnect
      // banner, no composer lockdown. The next prompt POST wakes the
      // worker server-side (`touch_and_wake_if_sunk` clears dormancy,
      // the reconciler respawns, `send_prompt` waits for it), so the
      // composer stays usable and `sendPrompt` / the drain effect read
      // `workerIdleStopped` to route a queued prompt through the POST
      // wake path instead of parking it forever. Cleared on the next
      // UserPromptSent or AcpSessionAssigned.
      next.workerIdleStopped = true;
      next.workerStopped = false;
      next.workerRestarting = false;
    }
    // Some upstream slash commands (e.g. /usage, /status, /memory in
    // claude-agent-acp) advertise via available_commands_update but
    // produce no agent_message_chunk and no tool calls when invoked;
    // see https://github.com/agentclientprotocol/claude-agent-acp/issues/642.
    // Detect that case and append a notice row. The `turnHasOutput`
    // flag is flipped by every output-producing handler and reset by
    // UserPromptSent, so this check is O(1) instead of walking the
    // full activity array on every Stopped.
    //
    // `state.turnActive` is read on the PRE-event state. Under the
    // counter derivation it means "at least one outstanding prompt
    // hasn't been retired yet," which is exactly what we want: it
    // skips spurious Stopped frames (no open turn to attribute the
    // notice to) and fires for the turn this Stopped is actually
    // retiring. In the race case, `turnHasOutput` still reflects the
    // turn being retired because UserPromptSent (which resets it) for
    // the follow-up hasn't been applied yet.
    if (state.turnActive && !state.turnHasOutput) {
      next.activity = pushActivity(next.activity, {
        id: `empty-${frame.seq}`,
        kind: "empty_output",
        text: "Command produced no output.",
        at: new Date().toISOString(),
      });
    }
    return next;
  }
  if ("IncompatibleAgent" in event) {
    // The cockpit refused to enter the session because the adapter
    // failed the per-adapter compatibility check (see
    // src/cockpit/agent_compat.rs). The structured payload powers the
    // dedicated StartupErrorScreen which short-circuits normal session
    // rendering. The parallel AgentStartupError event populates
    // `startupError` so legacy status logic still flips into Error.
    next.incompatibleAgent = event.IncompatibleAgent.detail;
    next.inFlightTool = null;
    sweepOpenToolCalls(next, frame.seq);
    next.agentUnresponsive = false;
    next.lastStoppedSeq = Math.min(
      next.lastStoppedSeq + 1,
      next.pendingUserPromptSeq,
    );
    next.turnActive = isTurnActive(next);
    return next;
  }
  if ("AgentStartupError" in event) {
    next.startupError = event.AgentStartupError.message;
    next.inFlightTool = null;
    sweepOpenToolCalls(next, frame.seq);
    // A failed respawn supersedes any in-progress unresponsive
    // escalation; the user sees the startup error banner instead.
    next.agentUnresponsive = false;
    // Same race-safe semantics as `Stopped`: advance `lastStoppedSeq`
    // by one so a startup failure for the prior turn doesn't kill the
    // spinner for a freshly-typed follow-up the user has already
    // submitted. See #1170.
    next.lastStoppedSeq = Math.min(
      next.lastStoppedSeq + 1,
      next.pendingUserPromptSeq,
    );
    next.turnActive = isTurnActive(next);
    return next;
  }
  if ("PromptCapabilities" in event) {
    const c = event.PromptCapabilities;
    next.promptCapabilities = {
      image: c.image,
      audio: c.audio,
      embeddedContext: c.embedded_context,
    };
    return next;
  }
  if ("UserPromptSent" in event) {
    const text = event.UserPromptSent.text;
    // Map server attachment refs to render-ready attachments backed by
    // the replay GET endpoint. Used on the replay/no-optimistic path;
    // the optimistic row already carries local preview URLs. See #1000.
    const serverAttachments: CockpitAttachment[] = (
      event.UserPromptSent.attachments ?? []
    ).map((a) => ({
      id: a.id,
      kind: a.kind,
      mimeType: a.mime_type,
      name: a.name,
      size: a.size,
      url: `/api/sessions/${encodeURIComponent(
        frame.session_id,
      )}/cockpit/attachments/${encodeURIComponent(a.id)}`,
    }));
    // Dedupe against the optimistic row that useCockpit's sendPrompt
    // dispatched a moment ago: find the OLDEST matching un-promoted
    // user_prompt with the same text and promote it to the
    // authoritative seq-based id. Walking oldest-first matters when
    // the user submits the same text twice in quick succession; the
    // first server echo must promote the first optimistic row, not
    // the second, so the seq order matches the submission order.
    const matchIdx = next.activity.findIndex(
      (r) =>
        r.kind === "user_prompt" &&
        r.text === text &&
        !r.id.startsWith("user-seq-"),
    );
    if (matchIdx >= 0) {
      // Optimistic-match path: promote the placeholder's id. The
      // client's `user_prompt` action already bumped
      // `pendingUserPromptSeq`, so we don't bump again here. The
      // per-turn resets below STILL apply: `turnHasOutput`, the
      // worker banners, and the wakeup countdown all reset on every
      // server-confirmed UserPromptSent regardless of which branch
      // promoted the row. See #1170.
      const match = next.activity[matchIdx];
      if (match) {
        const updated = next.activity.slice();
        // Keep the optimistic local previews if present (no refetch);
        // otherwise adopt the server refs so the bubble still renders.
        updated[matchIdx] = {
          ...match,
          id: `user-seq-${frame.seq}`,
          attachments:
            match.attachments && match.attachments.length > 0
              ? match.attachments
              : serverAttachments.length > 0
                ? serverAttachments
                : undefined,
        };
        next.activity = updated;
      }
    } else {
      // No optimistic row matched: this is a server-confirmed prompt
      // the client didn't dispatch (replay path, server-initiated, or
      // user action without optimistic local dispatch). Append a fresh
      // row and bump the prompt counter so `turnActive` derives true.
      // The optimistic-match branch above is reached when the client's
      // `user_prompt` action already bumped the counter; bumping again
      // here would double-count. See #1170.
      next.activity = pushActivity(next.activity, {
        id: `user-seq-${frame.seq}`,
        kind: "user_prompt",
        text,
        attachments: serverAttachments.length > 0 ? serverAttachments : undefined,
        at: new Date().toISOString(),
      });
      next.pendingUserPromptSeq = next.pendingUserPromptSeq + 1;
    }
    applyNewTurnResets(next);
    return next;
  }
  if ("UserDiffCommentsPrompt" in event) {
    // The "Send diff comments" dialog posts directly (no optimistic
    // row), so there is never a placeholder to promote: always append a
    // typed `user_diff_comments` row. `text` carries the assembled
    // markdown (agent-visible body / fallback); `diffComments` carries
    // the structured payload the runtime hands to the transcript card.
    const p = event.UserDiffCommentsPrompt;
    next.activity = pushActivity(next.activity, {
      id: `user-seq-${frame.seq}`,
      kind: "user_diff_comments",
      text: p.assembledMarkdown,
      diffComments: {
        intro: p.intro,
        outro: p.outro,
        isMultiRepo: p.isMultiRepo,
        comments: p.comments,
      },
      at: new Date().toISOString(),
    });
    next.pendingUserPromptSeq = next.pendingUserPromptSeq + 1;
    applyNewTurnResets(next);
    return next;
  }
  if ("AcpSessionAssigned" in event) {
    // Primary purpose: persistence breadcrumb so the server-side
    // listener can write the id to sessions.json for a subsequent
    // session/load.
    //
    // Secondary purpose: signal that the agent connection is alive
    // again. After a crash + respawn (e.g. the agent process was killed
    // and the supervisor restarted it), the prior turn's
    // AgentStartupError sat in SQLite and kept `startupError` set even
    // though the agent had since recovered. Clear sticky error flags
    // here so the red "Cockpit agent failed to start" banner heals on
    // its own once the respawn completes the handshake.
    next.startupError = null;
    next.lastError = null;
    // A fresh agent that passed the compatibility check has come
    // online; the structured incompatibility banner heals so the
    // session can resume.
    next.incompatibleAgent = null;
    // A fresh agent (via POST /cockpit/spawn after `aoe cockpit stop`
    // or via the reconciler's auto-respawn after `aoe cockpit restart`)
    // is online; clear both transient worker banners.
    next.workerStopped = false;
    next.workerRestarting = false;
    // The respawn may have been triggered by waking an idle-dormant
    // worker; the fresh handshake means it is no longer dormant.
    next.workerIdleStopped = false;
    // The respawn after an `agent_unresponsive` escalation completed;
    // clear the banner so the user can interact again. See #1196.
    next.agentUnresponsive = false;
    // Same shape for `prompt_orphaned`: the silent-orphan watchdog
    // fired, the runner was SIGTERMed, and the respawn handshake has
    // now landed. See #1240.
    next.agentOrphaned = false;
    return next;
  }
  if ("SessionContextReset" in event) {
    // session/load failed and the agent fell back to session/new; its
    // context window is empty. Clear the now-stale token-usage hint so
    // the composer footer doesn't keep showing the previous run's
    // "75k / 200k" until the next UsageUpdate arrives.
    next.sessionUsage = null;
    // The new ACP session restarts the agent-side cumulative cost at
    // zero, so any prior per-clear baseline no longer maps onto
    // incoming UsageUpdate values. See #1354.
    next.usageBaseline = null;
    // Suppress the visible notice on a session that never saw a user
    // prompt: claude-agent-acp doesn't persist a 0-prompt session, so
    // session/load failing on the next spawn is expected, not an
    // incident the user needs to know about. Events arrive in seq
    // order, so checking `activity` here captures "any prompt with a
    // lower seq than this reset"; later prompts won't retroactively
    // surface the suppressed row.
    const hasPriorPrompt = next.activity.some(
      (r) => r.kind === "user_prompt" || r.kind === "user_diff_comments",
    );
    if (!hasPriorPrompt) {
      return next;
    }
    next.activity = pushActivity(next.activity, {
      id: `reset-${frame.seq}`,
      kind: "context_reset",
      text:
        event.SessionContextReset.reason ||
        "Conversation context reset; agent transcript was unavailable.",
      at: new Date().toISOString(),
    });
    // Offer the opt-in primer affordance. The banner only appears
    // when there is a prior user prompt (we're already inside that
    // branch), and stays one-shot: any UserPromptSent below clears
    // it, even if the user typed something other than the primer.
    next.contextPrimerAvailable = {
      resetSeq: frame.seq,
      reason:
        event.SessionContextReset.reason ||
        "Conversation context reset; agent transcript was unavailable.",
    };
    return next;
  }
  if ("WakeupScheduled" in event) {
    next.nextWakeupAt = event.WakeupScheduled.at;
    next.nextWakeupReason = event.WakeupScheduled.reason ?? null;
    return next;
  }
  if ("CancelRequested" in event) {
    // aoe sent session/cancel and armed the escalation watchdog; the
    // turn is still active. Surface "Stopping..." and the escalation
    // deadline so the user gets feedback instead of a silent spinner,
    // and can reveal the Force-stop affordance. See #1727.
    next.cancelling = true;
    next.cancelEscalatesAt = event.CancelRequested.escalates_at;
    return next;
  }
  if ("AgentSwitched" in event) {
    // ACP backend handoff completed (e.g. claude -> codex after a
    // rate-limit). Drop everything tied to the prior backend so the
    // composer/footer don't keep showing Claude's usage bar, mode
    // pills, or in-flight tool card while talking to Codex. The
    // transcript stays intact on the event log; only the visible
    // overlay state is dropped. Append a session-divider row so the
    // UI shows where the handoff happened. See #1282.
    const { from, to, reason } = event.AgentSwitched;
    const now = new Date().toISOString();
    next.agent = to;
    next.rateLimit = null;
    next.inFlightTool = null;
    // Close any tool the prior backend left open before appending the
    // divider, so the transcript order is start -> stopped -> divider.
    sweepOpenToolCalls(next, frame.seq);
    next.thinking = false;
    next.pendingApprovals = [];
    next.sessionUsage = null;
    // The new backend reports its own cumulative cost starting from
    // zero, so the prior agent's per-clear baseline does not apply.
    // See #1354.
    next.usageBaseline = null;
    next.availableCommands = [];
    next.availableModes = [];
    next.currentModeId = null;
    next.plan = null;
    next.mode = "Default";
    next.startupError = null;
    next.lastAgentSwitch = { from, to, reason, at: now };
    // The switch path emits Stopped { user_stopped } from the
    // shutdown of the prior backend just before AgentSwitched, which
    // flips workerStopped/agentUnresponsive on. Without an explicit
    // clear here the user sees a "worker stopped / reconnecting"
    // banner on top of a freshly switched session during the new
    // agent's session/new handshake (until AcpSessionAssigned clears
    // it). Clear them eagerly so the banner stays hidden.
    next.workerStopped = false;
    next.workerRestarting = false;
    next.agentUnresponsive = false;
    // Per-adapter selectors belong to the previous backend; the new
    // backend will publish its own snapshot. See #1403.
    next.configOptions = [];
    next.configOptionSwitchFailed = null;
    next.pendingConfigOption = null;
    next.activity = [
      ...next.activity,
      {
        id: `agent-switched-${frame.seq}`,
        kind: "session_cleared",
        text: `Switched cockpit agent from ${from} to ${to} (${reason}).`,
        at: now,
      },
    ];
    return next;
  }
  if ("PromptRejected" in event) {
    // Daemon refused the follow-up prompt because another `session/prompt`
    // was still in flight. The rejected text has already been persisted
    // upstream as `UserPromptSent` by the REST handler; this event tells
    // the UI the daemon never forwarded it to the agent. Show a Retry
    // pill so the user can re-dispatch via the normal sendPrompt path
    // instead of having their message vanish silently. See #1196.
    const entry: RejectedPrompt = {
      id: `rejected-${frame.seq}`,
      text: event.PromptRejected.text,
      reason: event.PromptRejected.reason,
      rejectedAt: new Date().toISOString(),
    };
    const REJECTED_PROMPTS_CAP = 5;
    next.rejectedPrompts = [...next.rejectedPrompts, entry].slice(
      -REJECTED_PROMPTS_CAP,
    );
    // Retire the spinner for this rejected submission so the composer
    // unlocks. `pendingUserPromptSeq` was bumped by the optimistic
    // dispatch; advancing `lastStoppedSeq` by one (capped) gives this
    // rejection the same turn-retirement semantics as a Stopped without
    // letting it spill into a different turn's bookkeeping. See #1170
    // for the cap rationale.
    next.lastStoppedSeq = Math.min(
      next.lastStoppedSeq + 1,
      next.pendingUserPromptSeq,
    );
    next.turnActive = isTurnActive(next);
    return next;
  }
  // RawAgentUpdate, TodoListUpdated, anything else: pass through with
  // no state mutation. The activity feed shows the raw text where
  // useful via the catch-all branch in the UI.
  return next;
}

function pushActivity(rows: ActivityRow[], row: ActivityRow): ActivityRow[] {
  const next = rows.concat(row);
  if (activityLimit > 0 && next.length > activityLimit) {
    return next.slice(next.length - activityLimit);
  }
  return next;
}

/** Close any `tool_start` rows that never received a matching terminal
 *  row by synthesizing a `tool_stopped` row for each. Called from the
 *  turn-ending reducer arms (`Stopped`, `AgentSwitched`,
 *  `IncompatibleAgent`, `AgentStartupError`): once the turn ends, no
 *  tool that was part of it can still be running, yet the card status
 *  is derived from the paired terminal row (see ToolCards `statusFor`),
 *  not from the `inFlightTool` pointer the arms already null. Without
 *  this sweep an interrupted tool's card sticks on "running" with a
 *  live-ticking timer forever, live and on reload (the trailing
 *  `Stopped` is persisted and replayed through this same reducer). The
 *  case is reason-independent: even a `prompt_complete` with a dangling
 *  open tool (the agent forgot to emit a completion) is "stopped", not
 *  "done" or "failed", because the tool's real outcome was never
 *  reported. Any text streamed via `ToolCallContent` before the stop is
 *  drained into the synthesized row so it is not lost. See #1646. */
function sweepOpenToolCalls(next: CockpitState, frameSeq: number): void {
  const terminal = new Set<string>();
  for (const row of next.activity) {
    if (
      (row.kind === "tool_complete" ||
        row.kind === "tool_error" ||
        row.kind === "tool_stopped") &&
      row.toolCallId
    ) {
      terminal.add(row.toolCallId);
    }
  }
  const now = new Date().toISOString();
  let activity = next.activity;
  let outputs = next.toolOutputs;
  let drained = false;
  // Iterate the pre-sweep snapshot; `pushActivity` returns fresh arrays
  // assigned to the local `activity`, so the loop never sees the rows it
  // appends. Dedupe by `toolCallId` because pre-fix stores can carry
  // duplicate `tool_start` rows for one call.
  for (const row of next.activity) {
    if (row.kind !== "tool_start" || !row.toolCallId) continue;
    const id = row.toolCallId;
    if (terminal.has(id)) continue;
    terminal.add(id);
    const buffered = outputs[id] ?? "";
    if (buffered) {
      const { [id]: _drop, ...rest } = outputs;
      void _drop;
      outputs = rest;
      drained = true;
    }
    activity = pushActivity(activity, {
      id: `stopped-${id}-${frameSeq}`,
      kind: "tool_stopped",
      text: buffered,
      toolCallId: id,
      at: now,
    });
  }
  next.activity = activity;
  if (drained) next.toolOutputs = outputs;
}

/** Derived `turnActive` from the prompt / stop seq counters. Exported
 *  so any new consumer can compute it from the counters directly; the
 *  reducer also calls this to keep `state.turnActive` in lockstep so
 *  existing `state.turnActive` reads stay correct. See #1170.
 *
 *  Invariant: `lastStoppedSeq <= pendingUserPromptSeq` always holds.
 *  Both counters start at 0; `pendingUserPromptSeq` increments by one
 *  on every dispatched user prompt, and `lastStoppedSeq` advances by
 *  one per `Stopped` / `AgentStartupError` but is capped at
 *  `pendingUserPromptSeq` so spurious extra Stopped frames cannot
 *  poison a future turn. */
export function isTurnActive(
  state: Pick<CockpitState, "pendingUserPromptSeq" | "lastStoppedSeq">,
): boolean {
  return state.pendingUserPromptSeq > state.lastStoppedSeq;
}

/** Normalise a partial CockpitState so the turn counters are populated.
 *  Used by the localStorage loader after the #1170 schema change: pre-
 *  schema persisted entries have no counters, so we backfill from the
 *  cached `turnActive` boolean (true → one outstanding prompt, false →
 *  fully retired) and re-derive `turnActive` from the counters. */
export function normaliseTurnCounters(
  state: CockpitState & {
    pendingUserPromptSeq?: number;
    lastStoppedSeq?: number;
    rejectedPrompts?: RejectedPrompt[];
    agentUnresponsive?: boolean;
    agentOrphaned?: boolean;
    usageBaseline?: { cost: number } | null;
    configOptions?: ConfigOptionDescriptor[];
    configOptionSwitchFailed?: ConfigOptionSwitchFailure | null;
    pendingConfigOption?: { configId: string; value: string } | null;
  },
): CockpitState {
  const pendingUserPromptSeq =
    typeof state.pendingUserPromptSeq === "number"
      ? state.pendingUserPromptSeq
      : state.turnActive
        ? 1
        : 0;
  const lastStoppedSeq =
    typeof state.lastStoppedSeq === "number"
      ? state.lastStoppedSeq
      : state.turnActive
        ? 0
        : pendingUserPromptSeq;
  // Pre-#1196 persisted entries lack rejectedPrompts / agentUnresponsive;
  // backfill so the reducer and renderers see well-typed values instead
  // of `undefined` (which crashes RejectedPromptsStrip's `.length` read).
  const rejectedPrompts = Array.isArray(state.rejectedPrompts)
    ? state.rejectedPrompts
    : [];
  const agentUnresponsive =
    typeof state.agentUnresponsive === "boolean"
      ? state.agentUnresponsive
      : false;
  // Pre-#1240 persisted entries lack agentOrphaned; backfill to false
  // so the reducer and renderers see a well-typed value instead of
  // `undefined`.
  const agentOrphaned =
    typeof state.agentOrphaned === "boolean" ? state.agentOrphaned : false;
  // Pre-#1354 persisted entries lack usageBaseline; backfill to null
  // so the UsageUpdated reducer's `next.usageBaseline && ...` check
  // sees a well-typed value. The baseline stays null until the next
  // SessionCleared / ConversationCompacted, which matches the
  // pre-fix behaviour for that one session; subsequent /clear events
  // start subtracting normally.
  const usageBaseline =
    state.usageBaseline === undefined ? null : state.usageBaseline;
  // Pre-#1403 persisted entries lack the config-option trio.
  const configOptions = Array.isArray(state.configOptions)
    ? state.configOptions
    : [];
  const configOptionSwitchFailed =
    state.configOptionSwitchFailed === undefined
      ? null
      : state.configOptionSwitchFailed;
  const pendingConfigOption =
    state.pendingConfigOption === undefined ? null : state.pendingConfigOption;
  return {
    ...state,
    rejectedPrompts,
    agentUnresponsive,
    agentOrphaned,
    usageBaseline,
    configOptions,
    configOptionSwitchFailed,
    pendingConfigOption,
    pendingUserPromptSeq,
    lastStoppedSeq,
    turnActive: isTurnActive({ pendingUserPromptSeq, lastStoppedSeq }),
  };
}

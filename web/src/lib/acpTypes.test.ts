import { describe, expect, it } from "vitest";

import { reducer as acpHookReducer } from "../hooks/useAcpSession";
import {
  applyEvent,
  applyReducedState,
  deriveTurnActive,
  emptyAcpState,
  hasActiveBackgroundAgent,
  isVisiblyBusy,
  normaliseTurnState,
  type AcpEvent,
  type AcpState,
  type BackgroundAgent,
  type ConfigOptionDescriptor,
  type ReducedState,
} from "./acpTypes";

const ev = (state: AcpState, seq: number, event: AcpEvent) => applyEvent(state, { session_id: "s-1", seq, event });
const fold = (state: AcpState, ...events: AcpEvent[]) => events.reduce((s, e) => ev(s, s.lastSeq + 1, e), state);

const prompt = (text = "hi", prompt_id?: string): AcpEvent => ({
  UserPromptSent: prompt_id ? { text, prompt_id } : { text },
});
const stopped = (reason: string): AcpEvent => ({ Stopped: { reason } });
const usage = (used: number, cost?: number, size = 200_000): AcpEvent => ({
  UsageUpdated: {
    usage: cost === undefined ? { used, size } : { used, size, cost: { amount: cost, currency: "USD" } },
  },
});
const reset = (reason = "session/load failed"): AcpEvent => ({ SessionContextReset: { reason } });
const assigned: AcpEvent = { AcpSessionAssigned: { acp_session_id: "fresh" } };
const switched: AcpEvent = { AgentSwitched: { from: "claude", to: "codex", reason: "rate_limited" } };
const diffComments: AcpEvent = {
  UserDiffCommentsPrompt: { intro: "a", outro: "b", isMultiRepo: true, comments: [], assembledMarkdown: "a\n" },
};
const incompatibleDetail = {
  kind: "incompatible_agent_version" as const,
  package_name: "@agentclientprotocol/claude-agent-acp",
  installed: "0.32.0",
  required: "0.39.0",
  install_command: "npm install -g @agentclientprotocol/claude-agent-acp@latest",
  auto_install: false,
};
const toolStart: AcpEvent = {
  ToolCallStarted: {
    tool_call: { id: "tc-1", name: "Read File", kind: "read", args_preview: "{}", started_at: "2026-01-01T00:00:00Z" },
  },
};
const future = new Date(Date.now() + 95_000).toISOString();
const past = new Date(Date.now() - 5_000).toISOString();

describe("applyEvent control state", () => {
  it.each(["plain", "diff comments"])("a %s prompt opens the turn without adding a row", (kind) => {
    const next = fold(emptyAcpState(), kind === "plain" ? prompt() : diffComments);
    expect(next).toMatchObject({ activity: [], serverTurnActive: true, turnActive: true, promptSeq: 1, lastSeq: 1 });
  });

  it("drops a frame at or below lastSeq", () => {
    const seeded: AcpState = { ...emptyAcpState(), lastSeq: 3 };
    expect(ev(seeded, 3, prompt())).toBe(seeded);
  });

  it.each(["plain", "diff comments"])("a %s prompt applies the per-turn resets", (kind) => {
    const stale: AcpState = {
      ...emptyAcpState(),
      startupError: "old",
      lastError: "old",
      workerStopped: true,
      workerRestarting: true,
      workerIdleStopped: true,
      agentUnresponsive: true,
      agentOrphaned: true,
      rateLimitRetriesExhausted: true,
      contextPrimerAvailable: { resetSeq: 1, reason: "x" },
      monitorArmed: true,
      monitorDescription: "watch",
    };
    expect(fold(stale, kind === "plain" ? prompt() : diffComments)).toMatchObject({
      startupError: null,
      lastError: null,
      workerStopped: false,
      workerRestarting: false,
      workerIdleStopped: false,
      agentUnresponsive: false,
      agentOrphaned: false,
      rateLimitRetriesExhausted: false,
      contextPrimerAvailable: null,
      monitorArmed: false,
      monitorDescription: null,
      turnActive: true,
    });
  });

  const transitions: [string, AcpEvent[], Partial<AcpState>][] = [
    ["AcpSessionAssigned touches no conversation state", [assigned], { lastSeq: 1, activity: [], sessionUsage: null }],
    ["user_stopped closes the turn", [prompt(), stopped("user_stopped")], { workerStopped: true, turnActive: false }],
    ["prompt_complete is not a worker stop", [prompt(), stopped("prompt_complete")], { workerStopped: false }],
    [
      "a spurious Stopped does not poison the next prompt",
      [prompt(), stopped("x"), stopped("x"), prompt()],
      { turnActive: true, promptSeq: 2 },
    ],
    [
      "user_stopped then restart_pending",
      [stopped("user_stopped"), stopped("restart_pending")],
      { workerStopped: false, workerRestarting: true },
    ],
    [
      "idle_auto_stop",
      [stopped("idle_auto_stop")],
      { workerIdleStopped: true, workerStopped: false, workerRestarting: false, turnActive: false },
    ],
    ["rate_limit_exhausted_retries", [stopped("rate_limit_exhausted_retries")], { rateLimitRetriesExhausted: true }],
    [
      "RateLimitAutoResumed ends the exhausted park",
      [stopped("rate_limit_exhausted_retries"), { RateLimitAutoResumed: { resets_at: future } }],
      { rateLimitRetriesExhausted: false },
    ],
    [
      "AgentSwitched ends the exhausted park",
      [stopped("rate_limit_exhausted_retries"), switched],
      { rateLimitRetriesExhausted: false },
    ],
    // The armed park: the composer treats a worker-down 503 as re-queueable
    // wherever the daemon would have sent, so this flag has to track the park
    // the daemon actually sends into, not only the cap.
    ["rate_limited", [stopped("rate_limited")], { rateLimitParked: true, rateLimitRetriesExhausted: false }],
    ["a prompt landing ends the armed park", [stopped("rate_limited"), prompt()], { rateLimitParked: false }],
    [
      "an auto-resume attempt does not end the armed park, matching the daemon",
      [stopped("rate_limited"), { RateLimitAutoResumed: { resets_at: future } }],
      { rateLimitParked: true },
    ],
    [
      "prompt_orphaned",
      [prompt(), stopped("prompt_orphaned")],
      { agentOrphaned: true, workerRestarting: true, workerStopped: false, agentUnresponsive: false },
    ],
    [
      "agent_unresponsive then prompt_orphaned",
      [stopped("agent_unresponsive"), stopped("prompt_orphaned")],
      { agentUnresponsive: false, agentOrphaned: true },
    ],
    [
      "prompt_orphaned then agent_unresponsive",
      [stopped("prompt_orphaned"), stopped("agent_unresponsive")],
      { agentOrphaned: false, agentUnresponsive: true, workerRestarting: true },
    ],
    [
      "prompt_orphaned then user_stopped",
      [stopped("prompt_orphaned"), stopped("user_stopped")],
      { agentOrphaned: false },
    ],
    [
      "prompt_orphaned then restart_pending",
      [stopped("prompt_orphaned"), stopped("restart_pending")],
      { agentOrphaned: false, workerRestarting: true },
    ],
    ...["user_stopped", "restart_pending", "idle_auto_stop", "prompt_orphaned"].map(
      (reason): [string, AcpEvent[], Partial<AcpState>] => [
        `AcpSessionAssigned clears the ${reason} banner`,
        [stopped(reason), assigned],
        { workerStopped: false, workerRestarting: false, workerIdleStopped: false, agentOrphaned: false },
      ],
    ),
    [
      "IncompatibleAgent",
      [{ IncompatibleAgent: { detail: incompatibleDetail } }],
      { incompatibleAgent: incompatibleDetail },
    ],
    [
      "AcpSessionAssigned heals IncompatibleAgent",
      [{ IncompatibleAgent: { detail: incompatibleDetail } }, assigned],
      { incompatibleAgent: null },
    ],
    [
      "AgentStartupError closes the turn",
      [prompt(), { AgentStartupError: { message: "boom" } }],
      { turnActive: false, startupError: "boom" },
    ],
    [
      "a reset after a prompt arms the primer",
      [usage(100), prompt(), reset("bad id")],
      { sessionUsage: null, contextPrimerAvailable: { resetSeq: 3, reason: "bad id" } },
    ],
    [
      "an empty reset reason gets a fallback",
      [prompt(), reset("")],
      { contextPrimerAvailable: { resetSeq: 2, reason: expect.stringMatching(/\S/) } },
    ],
    [
      "a reset before any prompt stays silent",
      [usage(100), reset()],
      { sessionUsage: null, contextPrimerAvailable: null, lastSeq: 2 },
    ],
    [
      "ConversationCompacted drops usage without arming the primer",
      [usage(100), "ConversationCompacted"],
      { activity: [], sessionUsage: null, contextPrimerAvailable: null },
    ],
    [
      "a mid-wait prompt keeps the pending wakeup",
      [{ WakeupScheduled: { at: future, reason: "wake" } }, prompt()],
      { nextWakeupAt: future, nextWakeupReason: "wake" },
    ],
    [
      "a prompt after the wake time clears it",
      [{ WakeupScheduled: { at: past, reason: "wake" } }, prompt()],
      { nextWakeupAt: null, nextWakeupReason: null },
    ],
    [
      "MonitorArmed",
      [{ MonitorArmed: { description: "clippy passes" } }],
      { monitorArmed: true, monitorDescription: "clippy passes" },
    ],
    [
      "a monitor persists through agent activity",
      [{ MonitorArmed: { description: "b" } }, "ThinkingStarted", { AgentMessageChunk: { text: "x" } }],
      { monitorArmed: true },
    ],
    [
      "a monitor persists past the arming turn's Stopped",
      [{ MonitorArmed: { description: "w" } }, stopped("prompt_complete")],
      { monitorArmed: true },
    ],
    ...["prompt_complete", "agent_idle"].map((reason): [string, AcpEvent[], Partial<AcpState>] => [
      `a fired monitor clears when the turn ends with ${reason}`,
      [{ MonitorArmed: { description: "w" } }, toolStart, stopped(reason)],
      { monitorArmed: false, monitorDescription: null },
    ]),
    [
      "ModeSwitchFailed",
      [{ ModeSwitchFailed: { mode_id: "bypassPermissions", reason: "denied" } }],
      { modeSwitchFailed: expect.objectContaining({ modeId: "bypassPermissions", reason: "denied" }) },
    ],
    [
      "CurrentModeChanged clears a mode switch failure",
      [{ ModeSwitchFailed: { mode_id: "b", reason: "d" } }, { CurrentModeChanged: { current_mode_id: "acceptEdits" } }],
      { modeSwitchFailed: null },
    ],
  ];

  it.each(transitions)("%s", (_name, events, expected) => {
    expect(fold(emptyAcpState(), ...events)).toMatchObject(expected);
  });

  it("codex /new drops usage to the post-reset baseline (#2979)", () => {
    let state = fold(emptyAcpState(), usage(75_000), prompt("/new"), "SessionCleared", reset("cleared"), assigned);
    expect(state).toMatchObject({ sessionUsage: null, usageBaseline: null });
    state = fold(state, stopped("session_reset"), usage(1_200));
    expect(state.turnActive).toBe(false);
    expect(state.sessionUsage?.used).toBe(1_200);
  });

  it("AgentSwitched records the handoff and clears backend state", () => {
    const seeded: AcpState = {
      ...emptyAcpState(),
      sessionUsage: { used: 100, size: 200_000 },
      usageBaseline: { cost: 4 },
      workerStopped: true,
      workerRestarting: true,
      agentUnresponsive: true,
    };
    expect(fold(seeded, switched)).toMatchObject({
      sessionUsage: null,
      usageBaseline: null,
      workerStopped: false,
      workerRestarting: false,
      agentUnresponsive: false,
      activity: [],
      lastAgentSwitch: { from: "claude", to: "codex", reason: "rate_limited" },
    });
  });
});

describe("cost baseline (#1354)", () => {
  it("SessionCleared adds the current cost to the baseline", () => {
    const seeded: AcpState = {
      ...emptyAcpState(),
      sessionUsage: { used: 10, size: 200_000, cost: { amount: 1.5, currency: "USD" } },
      usageBaseline: { cost: 2 },
    };
    expect(fold(seeded, "SessionCleared")).toMatchObject({ usageBaseline: { cost: 3.5 }, sessionUsage: null });
  });

  it.each<[string, AcpEvent[], number | null]>([
    ["captured by /clear", [usage(10_000, 0.42), "SessionCleared"], 0.42],
    ["zero for /clear with no prior usage", ["SessionCleared"], 0],
    ["stacked by repeated /clear", [usage(1, 0.1), "SessionCleared", usage(1, 0.15), "SessionCleared"], 0.15],
    ["captured by compaction", [usage(1, 0.3), "ConversationCompacted"], 0.3],
    [
      "stacked by compaction after /clear",
      [usage(1, 0.1), "SessionCleared", usage(1, 0.15), "ConversationCompacted"],
      0.15,
    ],
    ["reset by AgentSwitched", [usage(1, 0.42), "SessionCleared", switched], null],
    ["reset by SessionContextReset", [usage(1, 0.2), "SessionCleared", reset()], null],
  ])("baseline %s", (_name, events, baseline) => {
    const state = fold(emptyAcpState(), ...events);
    if (baseline === null) expect(state.usageBaseline).toBeNull();
    else expect(state.usageBaseline?.cost).toBeCloseTo(baseline, 6);
  });

  it.each<[string, AcpEvent[], number | null]>([
    ["subtracts the baseline", [usage(10_000, 0.42), "SessionCleared", usage(5_000, 0.49)], 0.07],
    ["leaves usage untouched with a zero baseline", ["SessionCleared", usage(1_000, 0.05)], 0.05],
    [
      "uses the accumulated baseline",
      [usage(1, 0.1), "SessionCleared", usage(1, 0.15), "SessionCleared", usage(1, 0.18)],
      0.03,
    ],
    ["subtracts a compaction baseline", [usage(1, 0.3), "ConversationCompacted", usage(1, 0.32)], 0.02],
    [
      "subtracts a stacked baseline",
      [usage(1, 0.1), "SessionCleared", usage(1, 0.15), "ConversationCompacted", usage(1, 0.17)],
      0.02,
    ],
    ["starts a switched backend at zero", [usage(1, 0.42), "SessionCleared", switched, usage(500, 0.01)], 0.01],
    ["passes a costless update through", [usage(1, 0.1), "SessionCleared", usage(1_000)], null],
    [
      "keeps the baseline across a costless update",
      [usage(1, 0.1), "SessionCleared", usage(1_000), usage(1_500, 0.12)],
      0.02,
    ],
    ["clamps a smaller cumulative to zero", [usage(1, 0.5), "SessionCleared", usage(100, 0.1)], 0],
  ])("UsageUpdated %s", (_name, events, cost) => {
    const u = fold(emptyAcpState(), ...events).sessionUsage;
    if (cost === null) expect(u?.cost ?? null).toBeNull();
    else expect(u?.cost?.amount).toBeCloseTo(cost, 6);
  });

  it("keeps the rest of the usage snapshot", () => {
    const u = fold(emptyAcpState(), usage(10_000, 0.42), "SessionCleared", usage(5_000, 0.49)).sessionUsage;
    expect(u).toMatchObject({ used: 5_000, size: 200_000, cost: { currency: "USD" } });
  });
});

describe("turnActive: daemon truth plus an optimistic overlay (#3417)", () => {
  const reduced = (turn_active: boolean): ReducedState => ({
    agent: "claude",
    model: null,
    mode: "Default",
    current_plan: null,
    in_flight_tool: null,
    pending_approvals: [],
    pending_elicitations: [],
    thinking: null,
    rate_limit: null,
    available_commands: [],
    available_modes: [],
    current_mode_id: null,
    turn_active,
    cancelling: false,
    compacting: false,
  });
  const steering: AcpEvent = {
    PromptCapabilities: { image: false, audio: false, embedded_context: false, steering: true },
  };

  it.each([
    [true, [], true],
    [false, [], false],
    [false, ["p1"], true],
    [true, ["p1"], true],
  ])("deriveTurnActive(%s, %o) is %s", (serverTurnActive, inflightPromptIds, expected) => {
    expect(deriveTurnActive({ serverTurnActive, inflightPromptIds })).toBe(expected);
  });

  it("hasActiveBackgroundAgent keys on endedAt, and isVisiblyBusy ORs it with turnActive", () => {
    const bg = (endedAt: string | null): BackgroundAgent => ({
      agentId: "a1",
      toolCallId: "tc1",
      description: "map backend",
      prompt: "do the thing",
      model: "claude-opus-4-8",
      status: endedAt ? "completed" : "running",
      startedAt: "2026-06-27T00:00:00Z",
      endedAt,
      toolCount: 0,
      tools: [],
      lastTool: null,
      lastText: null,
      result: null,
      warning: null,
    });
    const active = [bg(null)];

    expect(hasActiveBackgroundAgent({ backgroundAgents: [] })).toBe(false);
    expect(hasActiveBackgroundAgent({ backgroundAgents: active })).toBe(true);
    expect(hasActiveBackgroundAgent({ backgroundAgents: [bg("2026-06-27T00:00:10Z")] })).toBe(false);

    expect(isVisiblyBusy({ turnActive: false, backgroundAgents: [] })).toBe(false);
    expect(isVisiblyBusy({ turnActive: true, backgroundAgents: [] })).toBe(true);
    expect(isVisiblyBusy({ turnActive: false, backgroundAgents: active })).toBe(true);
  });

  it("a stalled background agent still reads as active, since Progress never sets endedAt (#4001)", () => {
    const launched: AcpEvent = {
      BackgroundAgentLaunched: {
        agent_id: "a1",
        tool_call_id: "tc1",
        description: "map backend",
        prompt: "do the thing",
        model: "claude-opus-4-8",
        started_at: "2026-06-27T00:00:00Z",
      },
    };
    const progress = (status: string, tool_count: number, at: string): AcpEvent => ({
      BackgroundAgentProgress: { agent_id: "a1", status, tool_count, at },
    });

    const stalled = fold(emptyAcpState(), launched, progress("stalled", 1, "2026-06-27T00:01:00Z"));
    expect(stalled.backgroundAgents[0].endedAt).toBeNull();
    expect(hasActiveBackgroundAgent(stalled)).toBe(true);

    const resumed = fold(stalled, progress("running", 2, "2026-06-27T00:01:05Z"));
    expect(resumed.backgroundAgents[0].endedAt).toBeNull();
    expect(hasActiveBackgroundAgent(resumed)).toBe(true);
  });

  it("N prompts steered into one turn are all closed by its single Stopped", () => {
    let state = fold(emptyAcpState(), steering);
    for (const id of ["p1", "p2", "p3", "p4", "p5"]) {
      state = acpHookReducer(state, { kind: "user_prompt", id, text: id });
      expect(state.turnActive).toBe(true);
      state = applyReducedState(fold(state, prompt(id, id)), reduced(true));
      expect(state.turnActive).toBe(true);
    }
    expect(state).toMatchObject({ inflightPromptIds: [], promptSeq: 5 });
    state = applyReducedState(fold(state, stopped("prompt_complete")), reduced(false));
    expect(state).toMatchObject({ turnActive: false, serverTurnActive: false });
  });

  it("a late Stopped from a prior turn does not clobber a fresh follow-up", () => {
    let state = fold(emptyAcpState(), prompt("first"));
    state = acpHookReducer(state, { kind: "user_prompt", id: "cmp-fu", text: "follow-up" });
    state = fold(state, stopped("prompt_complete"));
    expect(state).toMatchObject({ serverTurnActive: false, turnActive: true });
    expect(applyReducedState(state, reduced(false)).turnActive).toBe(true);
    state = fold(state, prompt("follow-up", "cmp-fu"));
    expect(state).toMatchObject({ inflightPromptIds: [], turnActive: true, promptSeq: 2 });
    expect(fold(state, stopped("prompt_complete")).turnActive).toBe(false);
  });

  it("an echo settles exactly its optimistic prompt id", () => {
    let state = acpHookReducer(emptyAcpState(), { kind: "user_prompt", id: "cmp-echo", text: "echo me" });
    expect(state.inflightPromptIds).toEqual(["cmp-echo"]);
    state = ev(state, 5, prompt("echo me", "cmp-echo"));
    expect(state).toMatchObject({ inflightPromptIds: [], turnActive: true, promptSeq: 1 });
  });

  it("an idle session's own first prompt is not a steered continuation", () => {
    let state: AcpState = { ...fold(emptyAcpState(), steering), workerStopped: true, workerRestarting: true };
    state = acpHookReducer(state, { kind: "user_prompt", id: "cmp-first", text: "first" });
    expect(state).toMatchObject({ turnActive: true, serverTurnActive: false });
    expect(fold(state, prompt("first", "cmp-first"))).toMatchObject({ workerStopped: false, workerRestarting: false });
  });

  it("a steered mid-turn prompt skips the per-turn resets", () => {
    const running: AcpState = {
      ...emptyAcpState(),
      promptCapabilities: { image: false, audio: false, embeddedContext: false, steering: true },
      serverTurnActive: true,
      turnActive: true,
      cancelEscalatesAt: future,
    };
    expect(fold(running, prompt("steered")).cancelEscalatesAt).toBe(future);
  });
});

describe("normaliseTurnState (persisted-state backfill)", () => {
  const persisted = (over: Partial<AcpState>, ...omit: (keyof AcpState)[]) => {
    const state: Partial<AcpState> = { ...emptyAcpState(), ...over };
    for (const key of omit) delete state[key];
    return state as AcpState;
  };
  const prompts = [
    { id: "a", kind: "user_prompt" as const, text: "one", at: "" },
    { id: "b", kind: "message" as const, text: "hi", at: "" },
    { id: "c", kind: "user_prompt" as const, text: "two", at: "" },
  ];

  it.each<[string, AcpState, Partial<AcpState>]>([
    [
      "seeds serverTurnActive from turnActive",
      persisted({ turnActive: true }, "serverTurnActive"),
      { serverTurnActive: true, turnActive: true },
    ],
    [
      "seeds an idle serverTurnActive",
      persisted({ turnActive: false }, "serverTurnActive"),
      { serverTurnActive: false, turnActive: false },
    ],
    [
      "never restores in-flight prompt ids",
      persisted({ turnActive: true, inflightPromptIds: ["cmp-stale"] }),
      { inflightPromptIds: [], turnActive: false },
    ],
    ["counts prompt rows into promptSeq", persisted({ activity: prompts }, "promptSeq"), { promptSeq: 2 }],
    [
      "backfills compactionReminderDismissed",
      persisted({}, "compactionReminderDismissed"),
      { compactionReminderDismissed: null },
    ],
    ["backfills agentOrphaned", persisted({}, "agentOrphaned"), { agentOrphaned: false }],
    ["backfills usageBaseline", persisted({}, "usageBaseline"), { usageBaseline: null }],
    ["keeps a usageBaseline", persisted({ usageBaseline: { cost: 0.42 } }), { usageBaseline: { cost: 0.42 } }],
  ])("%s", (_name, state, expected) => {
    expect(normaliseTurnState(state)).toMatchObject(expected);
  });
});

describe("compaction reminder dismissal", () => {
  it("survives climbing usage and re-arms after a context boundary", () => {
    let state = acpHookReducer(fold(emptyAcpState(), usage(160_000)), { kind: "dismiss_compaction_reminder" });
    state = fold(state, usage(180_000));
    expect(state.compactionReminderDismissed?.used).toBe(160_000);
    state = fold(state, "ConversationCompacted", usage(20_000));
    expect(state.compactionReminderDismissed).toBeNull();
    state = fold(acpHookReducer(state, { kind: "dismiss_compaction_reminder" }), usage(30_000));
    expect(state.compactionReminderDismissed?.used).toBe(20_000);
  });

  it.each<AcpEvent>(["ConversationCompacted", "SessionCleared", reset(), switched])("re-arms after %o", (boundary) => {
    const state = acpHookReducer(fold(emptyAcpState(), usage(160_000)), { kind: "dismiss_compaction_reminder" });
    expect(fold(state, boundary, usage(170_000)).compactionReminderDismissed).toBeNull();
  });
});

it.each([
  ["dismiss_primer", { contextPrimerAvailable: { resetSeq: 12, reason: "reset" } }, "contextPrimerAvailable"],
  ["dismiss_mode_switch_failed", { modeSwitchFailed: { modeId: "b", reason: "d", at: past } }, "modeSwitchFailed"],
] as const)("acpHookReducer %s clears its notice", (kind, seed, field) => {
  expect(acpHookReducer({ ...emptyAcpState(), ...seed }, { kind })[field]).toBeNull();
});

describe("config options (#1403)", () => {
  const model = (current_value: string): ConfigOptionDescriptor => ({
    id: "model",
    name: "Model",
    category: "model",
    current_value,
    options: [
      { value: "claude-opus-4-7", name: "Opus" },
      { value: "claude-sonnet-4-6", name: "Sonnet" },
    ],
  });
  const effort: ConfigOptionDescriptor = {
    id: "effort",
    name: "Reasoning Effort",
    category: "thought_level",
    current_value: "default",
    options: [{ value: "high", name: "High" }],
  };
  const options = (current = "claude-opus-4-7"): AcpEvent => ({
    ConfigOptionsUpdated: { options: [model(current), effort] },
  });
  const failed = (config_id = "model", value = "claude-sonnet-4-6"): AcpEvent => ({
    ConfigOptionSwitchFailed: { config_id, value, reason: "transient" },
  });

  it("replaces the whole snapshot", () => {
    const state = fold(emptyAcpState(), options(), { ConfigOptionsUpdated: { options: [model("claude-sonnet-4-6")] } });
    expect(state.configOptions).toEqual([model("claude-sonnet-4-6")]);
  });

  it("records a failure without touching the options", () => {
    const before = fold(emptyAcpState(), options());
    const state = fold(before, failed());
    expect(state.configOptions).toBe(before.configOptions);
    expect(state.configOptionSwitchFailed).toEqual({
      configId: "model",
      value: "claude-sonnet-4-6",
      reason: "transient",
      at: expect.any(String),
    });
  });

  it("a failure clears the pending click; a confirming snapshot clears the failure", () => {
    let state: AcpState = {
      ...emptyAcpState(),
      pendingConfigOption: { configId: "model", value: "claude-sonnet-4-6" },
    };
    state = fold(state, failed());
    expect(state.pendingConfigOption).toBeNull();
    expect(fold(state, options()).configOptionSwitchFailed).not.toBeNull();
    expect(fold(state, options("claude-sonnet-4-6")).configOptionSwitchFailed).toBeNull();
  });

  it("AgentSwitched clears options and the failure; SessionCleared keeps them", () => {
    const state = fold(emptyAcpState(), options(), failed("effort", "high"));
    expect(fold(state, switched)).toMatchObject({
      configOptions: [],
      configOptionSwitchFailed: null,
      pendingConfigOption: null,
    });
    expect(fold(state, "SessionCleared").configOptions).toHaveLength(2);
  });
});

describe("context-window latch (upstream claude-agent-acp #596)", () => {
  const modelIs = (current_value: string): AcpEvent => ({
    ConfigOptionsUpdated: { options: [{ id: "model", name: "Model", category: "model", current_value, options: [] }] },
  });

  it("keeps the largest window through a 200k downgrade", () => {
    const state = fold(emptyAcpState(), usage(10_000, undefined, 200_000), usage(20_000, undefined, 1_000_000));
    expect(fold(state, usage(30_000, undefined, 200_000)).sessionUsage).toMatchObject({
      size: 1_000_000,
      used: 30_000,
    });
  });

  it.each<[string, AcpEvent[]]>([
    ["a context boundary", ["SessionCleared"]],
    ["a model change", [modelIs("haiku")]],
  ])("resets on %s", (_name, boundary) => {
    let state = fold(emptyAcpState(), modelIs("sonnet"), usage(20_000, undefined, 1_000_000), ...boundary);
    expect(state.sessionUsage).toBeNull();
    state = fold(state, usage(5_000, undefined, 200_000));
    expect(state.sessionUsage?.size).toBe(200_000);
  });
});

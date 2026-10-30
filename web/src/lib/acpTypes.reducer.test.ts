import { describe, expect, it } from "vitest";
import { reducer } from "../hooks/acpSession/reducer";

import {
  appendElicitationAnswerRow,
  applyEvent,
  applyReducedState,
  webRendersServerRow,
  emptyAcpState,
  isVisiblyBusy,
  nativeInputAvailable,
  canSubmitPromptIntent,
  revokeNativeAuthority,
  mergeServerRows,
  patchServerRow,
  summarizeAnswers,
  transcriptRowToActivity,
  type AcpEvent,
  type AcpState,
  type Elicitation,
  type ReducedState,
  type TranscriptRow,
  type ToolCall,
} from "./acpTypes";

const ev = (state: AcpState, seq: number, event: AcpEvent) => applyEvent(state, { session_id: "s-1", seq, event });

function tc(id: string, over: Partial<ToolCall> = {}): ToolCall {
  return {
    id,
    name: "Bash",
    kind: "execute",
    args_preview: "{}",
    started_at: "2026-01-01T00:00:00Z",
    ...over,
  };
}

function reducedState(over: Partial<ReducedState> = {}): ReducedState {
  return {
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
    cancelling: false,
    turn_active: false,
    compacting: false,
    ...over,
  };
}

describe("webRendersServerRow", () => {
  it("skips notice rows and keeps everything else", () => {
    const row = (kind: string): TranscriptRow => ({
      id: `r-${kind}`,
      group_id: "g-1",
      kind: kind as TranscriptRow["kind"],
      at: "2026-01-01T00:00:00Z",
      text: "x",
    });
    expect(webRendersServerRow(row("notice"))).toBe(false);
    for (const kind of ["message", "user_prompt", "tool_start", "context_reset", "summary", "advisory"]) {
      expect(webRendersServerRow(row(kind))).toBe(true);
    }
  });
});

describe("applyReducedState (Tier 1.2)", () => {
  it("adopts the server's control state verbatim", () => {
    const approval = {
      nonce: "n-1",
      tool_call: tc("t-1"),
      destructive: true,
      requested_at: "2026-01-01T00:00:00Z",
    };
    const next = applyReducedState(
      emptyAcpState(),
      reducedState({
        agent: "codex",
        model: "gpt-5",
        mode: "Plan",
        current_plan: { plan_id: "p-1", version: 1, steps: [{ id: "s-1", title: "one", status: "Pending" }] },
        in_flight_tool: tc("t-9"),
        pending_approvals: [approval],
        thinking: { started_at: "2026-01-01T00:00:00Z" },
        rate_limit: { status: "limited", resets_at: null, kind: "usage" },
        available_commands: [{ name: "review", description: "Review", accepts_input: true }],
        available_modes: [{ id: "plan", name: "Plan" }],
        current_mode_id: "plan",
        cancelling: true,
        compacting: true,
      }),
    );
    expect(next.agent).toBe("codex");
    expect(next.model).toBe("gpt-5");
    expect(next.mode).toBe("Plan");
    expect(next.plan?.steps).toHaveLength(1);
    expect(next.inFlightTool?.id).toBe("t-9");
    expect(next.pendingApprovals).toEqual([approval]);
    expect(next.thinking).toBe(true);
    expect(next.rateLimit?.status).toBe("limited");
    expect(next.availableCommands).toHaveLength(1);
    expect(next.availableModes).toHaveLength(1);
    expect(next.currentModeId).toBe("plan");
    expect(next.cancelling).toBe(true);
    expect(next.compacting).toBe(true);
  });

  it("keeps omitted cold fields at their current value", () => {
    const seeded = applyReducedState(
      emptyAcpState(),
      reducedState({
        available_commands: [{ name: "review", description: "Review", accepts_input: false }],
        available_modes: [{ id: "plan", name: "Plan" }],
      }),
    );
    expect(seeded.availableCommands).toHaveLength(1);

    const omitted = applyReducedState(seeded, reducedState(), ["available_commands", "available_modes"]);
    expect(omitted.availableCommands).toHaveLength(1);
    expect(omitted.availableModes).toHaveLength(1);

    const cleared = applyReducedState(seeded, reducedState());
    expect(cleared.availableCommands).toHaveLength(0);
  });

  it("adopts the daemon's open background tasks, so one older than the replay window still reads busy", () => {
    // The replay window folds only the latest events, so the start of a
    // long-running task may never reach the reducer.
    const cold = applyReducedState(emptyAcpState(), reducedState({ background_tasks: ["t-1"] }));
    expect(cold.backgroundTasks).toEqual(["t-1"]);
    expect(isVisiblyBusy(cold)).toBe(true);

    // The daemon omits the field when nothing is open, which must clear it.
    const cleared = applyReducedState(cold, reducedState());
    expect(cleared.backgroundTasks).toEqual([]);
    expect(isVisiblyBusy(cleared)).toBe(false);
  });

  it("leaves lastSeq alone so the raw-frame dedupe still governs replay", () => {
    const seeded = { ...emptyAcpState(), lastSeq: 12 };
    expect(applyReducedState(seeded, reducedState()).lastSeq).toBe(12);
  });

  it("keeps a locally-resolved card hidden until the server drops it", () => {
    const approval = {
      nonce: "n-1",
      tool_call: tc("t-1"),
      destructive: false,
      requested_at: "2026-01-01T00:00:00Z",
    };
    const pending = reducedState({ pending_approvals: [approval] });
    let state = applyReducedState(emptyAcpState(), pending);
    expect(state.pendingApprovals).toHaveLength(1);

    state = { ...state, pendingApprovals: [], locallyResolved: ["n-1"] };
    state = applyReducedState(state, pending);
    expect(state.pendingApprovals).toEqual([]);

    state = applyReducedState(state, reducedState());
    expect(state.locallyResolved).toEqual([]);
    state = applyReducedState(state, pending);
    expect(state.pendingApprovals).toHaveLength(1);
  });
});

describe("summarizeAnswers (#2209)", () => {
  function question(field_key: string, kind: Elicitation["questions"][number]["kind"], title?: string) {
    return { field_key, title: title ?? null, required: false, kind, options: [] };
  }
  function form(questions: Elicitation["questions"]): Elicitation {
    return { nonce: "n", message: "m", questions, requested_at: "2026-01-01T00:00:00Z" };
  }

  it("renders every answer kind in question order, omitting unanswered fields and labelling untitled ones by key", () => {
    const elicitation = form([
      question("sel", "single_select", "Color"),
      question("multi", "multi_select", "Tags"),
      question("txt", "free_text", "Name"),
      question("flag_on", "boolean", "On"),
      question("flag_off", "boolean", "Off"),
      question("num", "number", "Score"),
      question("blank", "free_text"), // unanswered -> omitted
      question("question_0", "free_text"), // untitled -> field key
    ]);
    const out = summarizeAnswers(elicitation, {
      sel: "Blue",
      multi: ["a", "b"],
      txt: "Ada",
      flag_on: true,
      flag_off: false,
      num: 4,
      question_0: "hi",
    });
    expect(out).toEqual([
      { question: "Color", answer: "Blue" },
      { question: "Tags", answer: "a, b" },
      { question: "Name", answer: "Ada" },
      { question: "On", answer: "Yes" },
      { question: "Off", answer: "No" },
      { question: "Score", answer: "4" },
      { question: "question_0", answer: "hi" },
    ]);
  });

  it("maps select values to option labels (MCP token, and AskUserQuestion desc)", () => {
    const q = {
      field_key: "color",
      title: "Color",
      required: true,
      kind: "single_select" as const,
      options: [
        { value: "tok_blue", label: "Blue" }, // MCP: token -> human label
        { value: "Green", label: "Green \u2014 the color green" }, // AskUserQuestion: keep bare value
      ],
    };
    expect(summarizeAnswers(form([q]), { color: "tok_blue" })[0]!.answer).toBe("Blue");
    expect(summarizeAnswers(form([q]), { color: "Green" })[0]!.answer).toBe("Green");
  });
});

describe("appendElicitationAnswerRow (#2209)", () => {
  it("appends a keyed row, idempotent by id and skipped for empty answers", () => {
    const a = appendElicitationAnswerRow([], "n-1", [{ question: "q", answer: "a" }]);
    expect(a).toHaveLength(1);
    expect(a[0]!.id).toBe("elicitation-n-1");
    const b = appendElicitationAnswerRow(a, "n-1", [{ question: "q", answer: "a" }]);
    expect(b).toBe(a); // same ref, no duplicate
    expect(appendElicitationAnswerRow([], "n-1", [])).toEqual([]);
  });
});

describe("applyEvent / PromptCapabilities", () => {
  it.each([undefined, true, false])("maps wire fields with steering=%s (#2805)", (steering) => {
    const wire = { image: true, audio: false, embedded_context: true, ...(steering === undefined ? {} : { steering }) };
    expect(ev(emptyAcpState(), 1, { PromptCapabilities: wire }).promptCapabilities).toEqual({
      image: true,
      audio: false,
      embeddedContext: true,
      steering: steering ?? false,
    });
  });
});

describe("applyEvent / PromptRejected (#1196)", () => {
  const rejected = (text: string): AcpEvent => ({ PromptRejected: { reason: "another prompt in flight", text } });

  it("records a Retry pill and retires the spinner for that submission", () => {
    const state = ev(ev(emptyAcpState(), 1, { UserPromptSent: { text: "do thing" } }), 2, rejected("do thing"));
    expect(state.rejectedPrompts).toEqual([
      expect.objectContaining({ id: "rejected-2", text: "do thing", reason: "another prompt in flight" }),
    ]);
    expect(state.turnActive).toBe(false);
  });

  it("caps the rejected-prompts FIFO at 5 entries", () => {
    let state = emptyAcpState();
    for (let i = 0; i < 7; i++) state = ev(state, i + 1, rejected(`p${i}`));
    expect(state.rejectedPrompts.map((r) => r.text)).toEqual(["p2", "p3", "p4", "p5", "p6"]);
  });
});

describe("applyEvent / background agents", () => {
  const launched: AcpEvent = {
    BackgroundAgentLaunched: {
      agent_id: "a1",
      tool_call_id: "task-1",
      description: "map backend",
      prompt: "do it",
      model: "claude-opus-4-8",
      started_at: "2026-06-27T00:00:00Z",
    },
  };
  const progress = (status: string, tool_count: number, at: string, over = {}): AcpEvent => ({
    BackgroundAgentProgress: { agent_id: "a1", status, tool_count, at, ...over },
  });
  const completed: AcpEvent = {
    BackgroundAgentCompleted: { agent_id: "a1", status: "completed", result: "done", ended_at: "2026-06-27T00:00:10Z" },
  };
  const stalledTerminal: AcpEvent = {
    BackgroundAgentCompleted: { agent_id: "a1", status: "stalled", ended_at: "2026-06-27T00:00:10Z" },
  };
  const agent = (...events: AcpEvent[]) =>
    events.reduce((s, e, i) => ev(s, i + 1, e), emptyAcpState()).backgroundAgents;

  it("builds, updates, and finalizes a record", () => {
    expect(agent(launched)).toEqual([
      expect.objectContaining({ status: "running", toolCallId: "task-1", endedAt: null }),
    ]);
    const running = progress("running", 4, "2026-06-27T00:00:05Z", { last_tool: "Read", last_text: "scanning" });
    expect(agent(launched, running)[0]).toMatchObject({ toolCount: 4, lastTool: "Read" });
    expect(agent(launched, running, completed)[0]).toMatchObject({ status: "completed", result: "done" });
  });

  it("leaves endedAt null on a stall, so the elapsed timer keeps ticking (#4001)", () => {
    const stalled = progress("stalled", 1, "2026-06-27T00:01:30Z");
    expect(agent(launched, stalled)[0]).toMatchObject({ status: "stalled", endedAt: null });
    const resumed = agent(launched, stalled, progress("running", 2, "2026-06-27T00:01:35Z"))[0];
    expect(resumed).toMatchObject({ status: "running", endedAt: null });
  });

  it("does not reopen a completed or stalled-terminal agent on a late progress event (#4001)", () => {
    const lateProgress = progress("running", 99, "2026-06-27T00:00:20Z");
    expect(agent(launched, completed, lateProgress)[0]).toMatchObject({ status: "completed", toolCount: 0 });
    // A terminal BackgroundAgentCompleted can carry status "stalled" (the tailer's
    // own abort timeout), so the endedAt guard, not the status list, has to stop it.
    const late = agent(launched, stalledTerminal, lateProgress)[0];
    expect(late).toMatchObject({ status: "stalled", toolCount: 0, endedAt: "2026-06-27T00:00:10Z" });
  });
});

describe("transcriptRowToActivity (Tier 4 wire mapping)", () => {
  it("maps snake_case wire fields to the camelCase ActivityRow shape", () => {
    const row = transcriptRowToActivity(
      {
        id: "done-t-1",
        group_id: "tool-t-1",
        kind: "tool_complete",
        at: "2026-01-01T00:00:00Z",
        text: "ok",
        tool_call_id: "t-1",
        output: [{ kind: "text", text: "hi" }],
        async_subagent: true,
      },
      "s-1",
    );
    expect(row.toolCallId).toBe("t-1");
    expect(row.output).toEqual([{ kind: "text", text: "hi" }]);
    expect(row.asyncSubagent).toBe(true);
  });

  it("carries the sending plugin of a user prompt and omits it otherwise", () => {
    const base = { id: "u-1", group_id: "g-1", kind: "user_prompt" as const, at: "2026-01-01T00:00:00Z", text: "hi" };
    expect(transcriptRowToActivity({ ...base, origin_plugin: "cron" }, "s-1").originPlugin).toBe("cron");
    expect("originPlugin" in transcriptRowToActivity(base, "s-1")).toBe(false);
  });

  it("builds the replay-GET url for attachments and seeds raw_name from name", () => {
    const row = transcriptRowToActivity(
      {
        id: "start-t-1",
        group_id: "tool-t-1",
        kind: "tool_start",
        at: "2026-01-01T00:00:00Z",
        text: "Bash",
        tool_call_id: "t-1",
        tool: {
          id: "t-1",
          name: "Bash",
          kind: "execute",
          args_preview: "{}",
          started_at: "2026-01-01T00:00:00Z",
        },
        attachments: [{ id: "att-1", kind: "image", mime_type: "image/png", name: "x.png", size: 9 }],
      },
      "s 1",
    );
    expect(row.tool?.raw_name).toBe("Bash");
    expect(row.attachments?.[0]!.url).toBe("/api/sessions/s%201/acp/attachments/att-1");
  });

  it("maps a diff_comments payload to the camelCase card shape", () => {
    const row = transcriptRowToActivity(
      {
        id: "user-seq-3",
        group_id: "g1",
        kind: "user_diff_comments",
        at: "2026-01-01T00:00:00Z",
        text: "# body",
        diff_comments: { intro: "look", outro: "thanks", is_multi_repo: true, comments: [] },
      },
      "s-1",
    );
    expect(row.diffComments).toEqual({ intro: "look", outro: "thanks", isMultiRepo: true, comments: [] });
  });
});

describe("mergeServerRows (Tier 4 reconcile-by-id)", () => {
  const start = (id: string, over: Partial<ToolCall> = {}) => ({
    id: `start-${id}`,
    kind: "tool_start" as const,
    text: "Bash",
    toolCallId: id,
    at: "2026-01-01T00:00:00Z",
    tool: tc(id, over),
  });

  it("appends new rows in order and returns the same ref for an empty batch", () => {
    const existing = [start("a")];
    expect(mergeServerRows(existing, [])).toBe(existing);
    const merged = mergeServerRows(existing, [
      { id: "done-a", kind: "tool_complete", text: "ok", toolCallId: "a", at: "2026-01-01T00:00:01Z" },
    ]);
    expect(merged.map((r) => r.id)).toEqual(["start-a", "done-a"]);
  });

  it("replaces a non-tool row by id in place (server authoritative), idempotent on re-append", () => {
    const existing = [{ id: "msg-1", kind: "message" as const, text: "old", at: "2026-01-01T00:00:00Z" }];
    const merged = mergeServerRows(existing, [
      { id: "msg-1", kind: "message", text: "new", at: "2026-01-01T00:00:00Z" },
    ]);
    expect(merged).toHaveLength(1);
    expect(merged[0]!.text).toBe("new");
  });

  it("merges a sparse synth tool_start into a richer existing start at the seam (#2711)", () => {
    const existing = [start("a", { kind: "execute", args_preview: '{"x":1}' })];
    const sparse = {
      id: "start-a",
      kind: "tool_start" as const,
      text: "tool call",
      toolCallId: "a",
      at: "2026-01-01T00:00:00Z",
      tool: tc("a", { kind: "other", args_preview: "" }),
    };
    const merged = mergeServerRows(existing, [sparse]);
    expect(merged).toHaveLength(1);
    expect(merged[0]!.tool?.kind).toBe("execute");
    expect(merged[0]!.tool?.args_preview).toBe('{"x":1}');
  });
});

describe("patchServerRow (Tier 4 delta Patch)", () => {
  it("replaces the row by id, or appends when the id is not present", () => {
    const existing = [{ id: "start-a", kind: "tool_start" as const, text: "Bash", toolCallId: "a", at: "t" }];
    const patched = patchServerRow(existing, {
      id: "start-a",
      kind: "tool_start",
      text: "Terminal",
      toolCallId: "a",
      at: "t",
    });
    expect(patched[0]!.text).toBe("Terminal");
    const appended = patchServerRow(existing, { id: "msg-9", kind: "message", text: "hi", at: "t" });
    expect(appended.map((r) => r.id)).toEqual(["start-a", "msg-9"]);
  });
});

describe("operation failure evidence", () => {
  it("keeps legacy provenance unknown and retains a failed turn without a startup failure", () => {
    for (const evidence of [undefined, null] as const) {
      let state = ev(emptyAcpState(), 1, { UserPromptSent: { text: "first", prompt_id: "p1" } });
      state = ev(state, 2, { PromptRuntimeError: { message: "request failed", evidence } });
      state = ev(state, 3, { Stopped: { reason: "prompt_failed" } });
      expect(state.lastError).toBe("request failed");
      expect(state.lastErrorEvidence).toBeNull();
      expect(state.startupError).toBeNull();
      expect(state.turnActive).toBe(false);
      expect(state.workerStopped).toBe(false);
    }
  });

  it("adopts typed metadata without duplicating the diagnostic and clears it on the next authoritative prompt", () => {
    const evidence = {
      source: "local_prompt_result",
      operation: "prompt",
      runtime: "unknown",
      blocking_scope: "operation",
      reported_code: -32603,
      observed_at_ms: 100,
      generation: { acp_worker: 7 },
    } as const;
    let state = ev(emptyAcpState(), 1, { PromptRuntimeError: { message: "request failed", evidence } });
    state = ev(state, 2, { Stopped: { reason: "prompt_failed" } });
    expect(state.lastErrorEvidence).toEqual(evidence);
    state = applyReducedState(state, reducedState({ operation_error: evidence, turn_active: false }));
    expect(state.lastError).toBe("request failed");
    expect(state.lastErrorEvidence).toEqual(evidence);
    state = ev(state, 3, { AcpSessionAssigned: { acp_session_id: "same-native-history" } });
    expect(state.lastError).toBe("request failed");
    expect(state.lastErrorEvidence).toEqual(evidence);
    const retrying = reducer(state, { kind: "user_prompt", text: "second", id: "p2" });
    expect(retrying.lastError).toBe("request failed");
    expect(retrying.lastErrorEvidence).toEqual(evidence);
    state = retrying;
    state = ev(state, 4, { UserPromptSent: { text: "second", prompt_id: "p2" } });
    expect(state.lastError).toBeNull();
    expect(state.lastErrorEvidence).toBeNull();
    expect(state.turnActive).toBe(true);
  });
});

describe("current native snapshot authority", () => {
  const loss = { source: "native", cause: "stdout_eof", phase: "established" } as const;
  const snapshot = (state: AcpState, seq: number, over: Partial<ReducedState> = {}) =>
    applyReducedState(state, reducedState({ connection_ready: true, ...over }), [], seq);

  it("requires a genuine current receipt, including a valid seq-zero first snapshot", () => {
    let state = ev(emptyAcpState(), 1, { AcpSessionAssigned: { acp_session_id: "cached", connection_ready: true } });
    expect(nativeInputAvailable(state)).toBe(false);
    expect(canSubmitPromptIntent(state)).toBe(false);
    state = snapshot(emptyAcpState(), 0);
    expect(nativeInputAvailable(state)).toBe(true);
    expect(canSubmitPromptIntent(state)).toBe(true);
    for (const connection_ready of [undefined, null, false]) {
      const unknown = snapshot(emptyAcpState(), 0, { connection_ready });
      expect(nativeInputAvailable(unknown)).toBe(false);
      expect(canSubmitPromptIntent(unknown)).toBe(false);
    }
    expect(nativeInputAvailable(applyReducedState(emptyAcpState(), reducedState({ connection_ready: true })))).toBe(
      false,
    );
  });

  it("permits only the explicit current governor to submit an unknown-channel wake intent", () => {
    const parked = snapshot(emptyAcpState(), 0, { connection_ready: false, prompt_wake_eligible: true });
    expect(nativeInputAvailable(parked)).toBe(false);
    expect(canSubmitPromptIntent(parked)).toBe(true);
    const refused = snapshot(parked, 0, {
      connection_ready: false,
      prompt_wake_eligible: true,
      connection_unavailable: loss,
    });
    expect(canSubmitPromptIntent(refused)).toBe(false);
    expect(canSubmitPromptIntent(revokeNativeAuthority(parked))).toBe(false);
  });

  it("accepts authoritative same-sequence refreshes and rejects older snapshots and raw overrides", () => {
    let state = snapshot(emptyAcpState(), 10, { connection_ready: false, connection_unavailable: loss });
    state = snapshot(state, 10);
    expect(nativeInputAvailable(state)).toBe(true);
    state = ev(state, 9, { AcpConnectionUnavailable: loss });
    expect(nativeInputAvailable(state)).toBe(true);
    expect(state.connectionUnavailable).toBeNull();
    state = snapshot(state, 9, { connection_ready: false, connection_unavailable: loss });
    expect(nativeInputAvailable(state)).toBe(true);
    state = ev(state, 11, { AcpConnectionUnavailable: loss });
    expect(nativeInputAvailable(state)).toBe(false);
    expect(state.connectionUnavailable).toBeNull();
    state = ev(state, 12, { AcpSessionAssigned: { acp_session_id: "backfill", connection_ready: true } });
    expect(nativeInputAvailable(state)).toBe(false);
    expect(state.connectionUnavailableNotice).toEqual(loss);
    state = snapshot(state, 12);
    expect(nativeInputAvailable(state)).toBe(true);
    expect(state.connectionUnavailableNotice).toEqual(loss);
  });

  it("does not let covered history reopen the current turn or suppress its prompt echo", () => {
    let state = snapshot(emptyAcpState(), 10);
    state = reducer(state, { kind: "user_prompt", id: "mine", text: "explicit prompt" });
    state = { ...state, lastError: "previous failed request" };
    state = snapshot(state, 20, { turn_active: false });
    state = ev(state, 11, { UserPromptSent: { text: "explicit prompt", prompt_id: "mine" } });
    expect(state.inflightPromptIds).toEqual([]);
    expect(state.turnActive).toBe(false);
    expect(state.lastError).toBeNull();
    state = ev(state, 12, "ThinkingStarted");
    expect(state.turnActive).toBe(false);
    expect(state.thinking).toBe(false);
  });

  it.each(["available", "lost"] as const)(
    "uses the historical turn for steered replay metadata behind a current %s header",
    (availability) => {
      let state = ev(emptyAcpState(), 1, {
        PromptCapabilities: { image: false, audio: false, embedded_context: false, steering: true },
      });
      state = ev(state, 2, { UserPromptSent: { text: "first turn" } });
      delete state.replayTurnActive; // Existing caches have only their raw-cursor serverTurnActive.
      state = snapshot(state, 20, {
        connection_ready: availability === "available",
        connection_unavailable: availability === "lost" ? loss : null,
        turn_active: availability === "available",
      });
      const visibleTurn = state.turnActive;
      state = ev(state, 11, { Stopped: { reason: "rate_limit_exhausted_retries" } });
      state = ev(state, 12, { Stopped: { reason: "rate_limited" } });
      expect(state.rateLimitRetriesExhausted && state.rateLimitParked).toBe(true);
      expect(state.turnActive).toBe(visibleTurn);
      state = ev(state, 13, { UserPromptSent: { text: "genuine next turn" } });
      expect(state.rateLimitRetriesExhausted || state.rateLimitParked).toBe(false);
      expect(state.turnActive).toBe(visibleTurn);
      // A subsequent actual steered continuation must keep its current-turn metadata.
      state = ev(state, 14, { MonitorArmed: { description: "same-turn monitor" } });
      state = ev(state, 15, { UserPromptSent: { text: "steered continuation" } });
      expect(state.monitorArmed).toBe(true);
      expect(state.monitorDescription).toBe("same-turn monitor");
      expect(state.turnActive).toBe(visibleTurn);
      expect(nativeInputAvailable(state)).toBe(availability === "available");
      expect(state.pendingApprovals).toEqual([]);
      expect(state.pendingElicitations).toEqual([]);
    },
  );

  it.each(["approval", "elicitation"] as const)("settles only a matching covered %s acknowledgement", (kind) => {
    let state = snapshot(emptyAcpState(), 10);
    state = reducer(state, { kind: "error", message: "resolution failed", target: { kind, nonce: "current" } });
    state = ev(state, 1, { UserPromptSent: { text: "historical prompt" } });
    expect(state.interactionError?.message).toBe("resolution failed");
    const resolution = (nonce: string): AcpEvent =>
      kind === "approval"
        ? { ApprovalResolved: { nonce, decision: "Allow" } }
        : { ElicitationResolved: { nonce, outcome: "Declined" } };
    state = ev(state, 2, resolution("different"));
    expect(state.interactionError?.message).toBe("resolution failed");
    state = ev(state, 3, resolution("current"));
    expect(state.interactionError).toBeNull();
    expect(nativeInputAvailable(state)).toBe(true);
    expect(state.pendingApprovals).toEqual([]);
    expect(state.pendingElicitations).toEqual([]);
  });

  it.each(["available", "lost", "governed"] as const)(
    "replays snapshot-absent fields behind a %s header without replacing its current controls",
    (availability) => {
      const approval = { nonce: "current-a", tool_call: tc("current-t"), destructive: false, requested_at: "t" };
      const evidence = {
        source: "runner_completion",
        operation: "prompt",
        runtime: "unknown",
        observed_at_ms: 100,
      } as const;
      const option = { id: "model", name: "Model", category: "model", current_value: "m", options: [] };
      const history: AcpEvent[] = [
        { UserPromptSent: { text: "old turn", prompt_id: "old" } },
        { PromptCapabilities: { image: true, audio: false, embedded_context: true, steering: true } },
        { ConfigOptionsUpdated: { options: [option] } },
        { ConfigOptionSwitchFailed: { config_id: "model", value: "bad", reason: "unsupported" } },
        { ModeSwitchFailed: { mode_id: "bad", reason: "unsupported" } },
        { UsageUpdated: { usage: { used: 10, size: 100, cost: { amount: 3, currency: "USD" } } } },
        "ConversationCompacted",
        { UsageUpdated: { usage: { used: 5, size: 100, cost: { amount: 7, currency: "USD" } } } },
        { WakeupScheduled: { at: "2099-01-01T00:00:00Z", reason: "follow up" } },
        { MonitorArmed: { description: "watch" } },
        { ToolCallStarted: { tool_call: tc("old-t") } },
        { Stopped: { reason: "rate_limit_exhausted_retries" } },
        { Stopped: { reason: "rate_limited" } },
        { SessionContextReset: { reason: "stored context unavailable" } },
        { CancelRequested: { escalates_at: "2099-01-01T00:01:00Z" } },
        {
          BackgroundAgentLaunched: {
            agent_id: "bg",
            tool_call_id: "bg-t",
            description: "inspect",
            prompt: "inspect",
            model: "m",
            started_at: "2026-01-01T00:00:00Z",
          },
        },
        { BackgroundAgentProgress: { agent_id: "bg", status: "running", tool_count: 2, at: "2026-01-01T00:00:01Z" } },
        {
          BackgroundAgentCompleted: {
            agent_id: "bg",
            status: "completed",
            result: "done",
            ended_at: "2026-01-01T00:00:02Z",
          },
        },
        { PromptRejected: { text: "old rejection", reason: "busy" } },
        { PromptRuntimeError: { message: "old diagnostic" } },
        { AgentStartupError: { message: "old startup diagnostic" } },
        { AcpConnectionUnavailable: loss },
        { SessionNotice: { severity: "warning", title: "old notice" } },
        { BackgroundTaskStarted: { task_id: "old-task", description: "old task" } },
        { CurrentModeChanged: { current_mode_id: "old-mode" } },
        { AgentSwitched: { from: "claude", to: "codex", reason: "user" } },
        "SessionCleared",
      ];
      let raw: AcpState = { ...emptyAcpState(), pendingConfigOption: { configId: "model", value: "m" } };
      let covered = snapshot(raw, 100, {
        connection_ready: availability === "available",
        connection_unavailable: availability === "lost" ? loss : null,
        prompt_wake_eligible: availability === "governed",
        turn_active: availability === "available",
        pending_approvals: [approval],
        operation_error: evidence,
        background_tasks: ["current-task"],
      });
      const current = covered;
      // Qualified current loss legitimately retires an impossible pending config.
      raw = { ...raw, pendingConfigOption: covered.pendingConfigOption };
      // Check replay-only fields after every intermediate event, including
      // config acknowledgement and context cost boundaries.
      const replayFields = [
        "configOptions",
        "pendingConfigOption",
        "sessionUsage",
        "usageBaseline",
        "compactionReminderDismissed",
        "promptCapabilities",
        "rateLimitRetriesExhausted",
        "rateLimitParked",
        "startupError",
        "lastError",
        "promptSeq",
        "nextWakeupAt",
        "nextWakeupReason",
        "monitorArmed",
        "monitorWorkSeen",
        "monitorDescription",
        "contextPrimerAvailable",
        "backgroundAgents",
        "cancelEscalatesAt",
        "dismissedNoticeIds",
      ] as const;
      history.forEach((event, index) => {
        raw = ev(raw, index + 1, event);
        covered = ev(covered, index + 1, event);
        for (const field of replayFields) expect(covered[field], field).toEqual(raw[field]);
        for (const field of ["modeSwitchFailed", "configOptionSwitchFailed", "lastAgentSwitch"] as const) {
          const expected = raw[field];
          if (expected) expect(covered[field]).toEqual({ ...expected, at: expect.any(String) });
          else expect(covered[field]).toBeNull();
        }
        expect(covered.rejectedPrompts.map(({ id, text, reason }) => ({ id, text, reason }))).toEqual(
          raw.rejectedPrompts.map(({ id, text, reason }) => ({ id, text, reason })),
        );
        expect(covered.connectionReady).toBe(current.connectionReady);
        expect(covered.connectionUnavailable).toEqual(current.connectionUnavailable);
        expect(covered.promptWakeEligible).toBe(current.promptWakeEligible);
        expect(covered.turnActive).toBe(current.turnActive);
        expect(covered.pendingApprovals).toEqual(current.pendingApprovals);
        expect(covered.lastErrorEvidence).toEqual(evidence);
        expect(covered.sessionNotices).toEqual(current.sessionNotices);
        expect(covered.backgroundTasks).toEqual(["current-task"]);
      });
      expect(ev(covered, history.length, history[history.length - 1]!)).toBe(covered);
    },
  );

  it("preserves live responders across ordinary failure, but actual scoped loss retires impossible controls", () => {
    const approval = { nonce: "a", tool_call: tc("t"), destructive: false, requested_at: "t" };
    const elicitation = { nonce: "e", message: "pick", questions: [], requested_at: "t", resolved: null };
    let state = snapshot(emptyAcpState(), 0, { pending_approvals: [approval], pending_elicitations: [elicitation] });
    state = ev(state, 1, { PromptRuntimeError: { message: "request failed" } });
    state = ev(state, 2, { Stopped: { reason: "prompt_failed" } });
    expect(nativeInputAvailable(state)).toBe(true);
    expect(state.pendingApprovals).toEqual([approval]);
    expect(state.pendingElicitations).toEqual([elicitation]);
    const retrying = reducer(state, { kind: "user_prompt", id: "retry", text: "second" });
    expect(retrying.lastError).toBe("request failed");
    state = snapshot(retrying, 3, {
      connection_ready: false,
      connection_unavailable: loss,
      pending_approvals: [approval],
      pending_elicitations: [elicitation],
    });
    expect(state.pendingApprovals).toEqual([]);
    expect(state.pendingElicitations).toEqual([]);
    expect(state.lastError).toBe("request failed");
    expect(state.workerStopped).toBe(false);
  });

  it("keeps retained native work on management refusal without granting controls", () => {
    const approval = { nonce: "a", tool_call: tc("t"), destructive: false, requested_at: "t" };
    const elicitation = { nonce: "e", message: "pick", questions: [], requested_at: "t", resolved: null };
    const refusal = { source: "runner_record", cause: "control_protocol_unknown", phase: "establishing" } as const;
    const unobserved = snapshot({ ...emptyAcpState(), inflightPromptIds: ["local-only"] }, 9, {
      connection_ready: false,
      connection_unavailable: refusal,
      turn_active: false,
    });
    expect(unobserved.turnActive).toBe(false);
    expect(nativeInputAvailable(unobserved)).toBe(false);
    expect(canSubmitPromptIntent(unobserved)).toBe(false);
    let state = snapshot(emptyAcpState(), 10, {
      connection_ready: false,
      connection_unavailable: refusal,
      turn_active: true,
      in_flight_tool: tc("t"),
      pending_approvals: [approval],
      pending_elicitations: [elicitation],
    });
    expect(state.turnActive).toBe(true);
    expect(state.serverTurnActive).toBe(true);
    expect(state.inFlightTool?.id).toBe("t");
    expect(state.pendingApprovals).toEqual([approval]);
    expect(state.pendingElicitations).toEqual([elicitation]);
    expect(nativeInputAvailable(state)).toBe(false);
    expect(canSubmitPromptIntent(state)).toBe(false);
    state = ev(state, 1, { UserPromptSent: { text: "retained" } });
    state = ev(state, 2, { AcpConnectionUnavailable: refusal });
    expect(state.replayTurnActive).toBe(true);
    expect(state.turnActive).toBe(true);
    state = snapshot(state, 11, {
      connection_ready: false,
      connection_unavailable: loss,
      turn_active: true,
      pending_approvals: [approval],
      pending_elicitations: [elicitation],
    });
    expect(state.turnActive).toBe(false);
    expect(state.pendingApprovals).toEqual([]);
    expect(state.pendingElicitations).toEqual([]);
  });
});

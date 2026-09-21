import { describe, expect, it } from "vitest";

import {
  appendElicitationAnswerRow,
  applyEvent,
  applyReducedState,
  webRendersServerRow,
  emptyAcpState,
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
    for (const kind of ["message", "user_prompt", "tool_start", "context_reset", "summary"]) {
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

  it("renders every answer kind in question order, omitting unanswered fields", () => {
    const elicitation = form([
      question("sel", "single_select", "Color"),
      question("multi", "multi_select", "Tags"),
      question("txt", "free_text", "Name"),
      question("flag_on", "boolean", "On"),
      question("flag_off", "boolean", "Off"),
      question("num", "number", "Score"),
      question("blank", "free_text"), // unanswered -> omitted
    ]);
    const out = summarizeAnswers(elicitation, {
      sel: "Blue",
      multi: ["a", "b"],
      txt: "Ada",
      flag_on: true,
      flag_off: false,
      num: 4,
    });
    expect(out).toEqual([
      { question: "Color", answer: "Blue" },
      { question: "Tags", answer: "a, b" },
      { question: "Name", answer: "Ada" },
      { question: "On", answer: "Yes" },
      { question: "Off", answer: "No" },
      { question: "Score", answer: "4" },
    ]);
  });

  it("falls back to the field key when a question has no title", () => {
    const out = summarizeAnswers(form([question("question_0", "free_text")]), { question_0: "hi" });
    expect(out).toEqual([{ question: "question_0", answer: "hi" }]);
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
  it("appends a keyed row and is idempotent by id", () => {
    const a = appendElicitationAnswerRow([], "n-1", [{ question: "q", answer: "a" }]);
    expect(a).toHaveLength(1);
    expect(a[0]!.id).toBe("elicitation-n-1");
    const b = appendElicitationAnswerRow(a, "n-1", [{ question: "q", answer: "a" }]);
    expect(b).toBe(a); // same ref, no duplicate
  });

  it("is a no-op for empty answers", () => {
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

  it("does not reopen a completed agent on a late progress event", () => {
    const late = agent(launched, completed, progress("running", 99, "2026-06-27T00:00:20Z"))[0];
    expect(late).toMatchObject({ status: "completed", toolCount: 0 });
  });

  it("does not reopen a stalled-terminal agent on a late progress event (#4001)", () => {
    // A terminal BackgroundAgentCompleted can carry status "stalled" (the tailer's
    // own abort timeout), so the endedAt guard, not the status list, has to stop it.
    const late = agent(launched, stalledTerminal, progress("running", 99, "2026-06-27T00:00:20Z"))[0];
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

describe("applyEvent / UserPromptSent prompt counter (Tier 4)", () => {
  it("bumps promptSeq for a prompt this client did not dispatch", () => {
    const next = ev(emptyAcpState(), 1, { UserPromptSent: { text: "hi", prompt_id: "cmp-1" } });
    expect(next.promptSeq).toBe(1);
    expect(next.turnActive).toBe(true);
    expect(next.activity).toHaveLength(0);
  });

  it("does NOT double-bump when the echoed prompt_id is one we have in flight", () => {
    const seeded: AcpState = {
      ...emptyAcpState(),
      optimisticRows: [{ id: "cmp-1", kind: "user_prompt", text: "hi", at: "t" }],
      inflightPromptIds: ["cmp-1"],
      promptSeq: 1,
      turnActive: true,
    };
    const next = ev(seeded, 1, { UserPromptSent: { text: "hi", prompt_id: "cmp-1" } });
    expect(next.promptSeq).toBe(1);
    expect(next.inflightPromptIds).toEqual([]);
  });
});

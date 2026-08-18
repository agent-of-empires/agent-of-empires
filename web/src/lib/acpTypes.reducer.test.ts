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
  type AcpFrame,
  type AcpState,
  type Elicitation,
  type ReducedState,
  type TranscriptRow,
  type ToolCall,
} from "./acpTypes";

// Targets the AcpEvent variants and helper branches the canonical
// acpTypes.test.ts leaves cold: PromptCapabilities, PromptRejected, and the
// client-side halves of the elicitation path. Both projections are
// server-owned now: the transcript rows since Tier 4 and the control state
// since Tier 1.2, so the folds themselves are covered by the Rust
// `TranscriptModel` / `AcpState` tests. What is asserted here is the client's
// side of the boundary: how a `reduced_state` frame lands, and what the
// client still derives for itself.

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
  // The daemon emits `notice` rows so the native view can show a failed
  // startup or a refused mode switch inline. The web shows the same thing as
  // a banner from its own control state, so rendering the row too would
  // duplicate it.
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
    // The wire carries a thinking signal object; the client renders a flag.
    expect(next.thinking).toBe(true);
    expect(next.rateLimit?.status).toBe("limited");
    expect(next.availableCommands).toHaveLength(1);
    expect(next.availableModes).toHaveLength(1);
    expect(next.currentModeId).toBe("plan");
    expect(next.cancelling).toBe(true);
    expect(next.compacting).toBe(true);
  });

  // The server omits cold fields this socket already holds; they arrive as
  // empty defaults, so adopting them would blank the pickers mid-session.
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

    // A frame that does not name them is authoritative, including empty.
    const cleared = applyReducedState(seeded, reducedState());
    expect(cleared.availableCommands).toHaveLength(0);
  });

  it("leaves lastSeq alone so the raw-frame dedupe still governs replay", () => {
    const seeded = { ...emptyAcpState(), lastSeq: 12 };
    expect(applyReducedState(seeded, reducedState()).lastSeq).toBe(12);
  });

  // The card clears on the resolve POST rather than waiting for the
  // broadcast (#1821); without the filter the next frame would paint it
  // straight back, since the daemon has not folded the resolve yet.
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

    // Once the daemon stops listing it the nonce is forgotten, so a later
    // request reusing it is not swallowed.
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
  it("maps the wire snake_case fields onto camelCase capability flags", () => {
    const next = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        PromptCapabilities: { image: true, audio: false, embedded_context: true },
      },
    });
    expect(next.promptCapabilities).toEqual({
      image: true,
      audio: false,
      embeddedContext: true,
      // Absent on the wire: events persisted before #2805 have no
      // `steering` key and must not read as capable.
      steering: false,
    });
  });

  it("carries the steering flag through, both ways (#2805)", () => {
    for (const steering of [true, false]) {
      const next = applyEvent(emptyAcpState(), {
        session_id: "s-1",
        seq: 1,
        event: {
          PromptCapabilities: {
            image: false,
            audio: false,
            embedded_context: false,
            steering,
          },
        },
      });
      expect(next.promptCapabilities?.steering).toBe(steering);
    }
  });
});

describe("applyEvent / PromptRejected (#1196)", () => {
  function rejectFrame(seq: number, text: string): AcpFrame {
    return {
      session_id: "s-1",
      seq,
      event: { PromptRejected: { reason: "another prompt in flight", text } },
    };
  }

  it("records a Retry pill and retires the spinner for that submission", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "do thing" } },
    });
    expect(state.turnActive).toBe(true);
    state = applyEvent(state, rejectFrame(2, "do thing"));
    expect(state.rejectedPrompts).toHaveLength(1);
    expect(state.rejectedPrompts[0]).toMatchObject({
      id: "rejected-2",
      text: "do thing",
      reason: "another prompt in flight",
    });
    expect(state.turnActive).toBe(false);
  });

  it("caps the rejected-prompts FIFO at 5 entries", () => {
    let state: AcpState = { ...emptyAcpState(), pendingUserPromptSeq: 10 };
    for (let i = 0; i < 7; i++) {
      state = applyEvent(state, rejectFrame(i + 1, `p${i}`));
    }
    expect(state.rejectedPrompts).toHaveLength(5);
    expect(state.rejectedPrompts[0].text).toBe("p2");
    expect(state.rejectedPrompts[4].text).toBe("p6");
  });
});

describe("applyEvent / background agents", () => {
  it("builds, updates, and finalizes a background-agent record", () => {
    let s = emptyAcpState();
    s = applyEvent(s, {
      session_id: "s-1",
      seq: 1,
      event: {
        BackgroundAgentLaunched: {
          agent_id: "a1",
          tool_call_id: "task-1",
          description: "map backend",
          prompt: "do it",
          model: "claude-opus-4-8",
          started_at: "2026-06-27T00:00:00Z",
        },
      },
    });
    expect(s.backgroundAgents).toHaveLength(1);
    expect(s.backgroundAgents[0]!.status).toBe("running");
    expect(s.backgroundAgents[0]!.toolCallId).toBe("task-1");

    s = applyEvent(s, {
      session_id: "s-1",
      seq: 2,
      event: {
        BackgroundAgentProgress: {
          agent_id: "a1",
          status: "running",
          tool_count: 4,
          last_tool: "Read",
          last_text: "scanning",
          at: "2026-06-27T00:00:05Z",
        },
      },
    });
    expect(s.backgroundAgents[0]!.toolCount).toBe(4);
    expect(s.backgroundAgents[0]!.lastTool).toBe("Read");

    s = applyEvent(s, {
      session_id: "s-1",
      seq: 3,
      event: {
        BackgroundAgentCompleted: {
          agent_id: "a1",
          status: "completed",
          result: "done",
          ended_at: "2026-06-27T00:00:10Z",
        },
      },
    });
    expect(s.backgroundAgents[0]!.status).toBe("completed");
    expect(s.backgroundAgents[0]!.result).toBe("done");
  });

  it("freezes the elapsed timer when an agent stalls and clears it if it resumes", () => {
    let s = emptyAcpState();
    s = applyEvent(s, {
      session_id: "s-1",
      seq: 1,
      event: {
        BackgroundAgentLaunched: {
          agent_id: "a1",
          tool_call_id: "t1",
          description: "x",
          prompt: "y",
          model: "m",
          started_at: "2026-06-27T00:00:00Z",
        },
      },
    });
    expect(s.backgroundAgents[0]!.endedAt).toBeNull();
    s = applyEvent(s, {
      session_id: "s-1",
      seq: 2,
      event: {
        BackgroundAgentProgress: { agent_id: "a1", status: "stalled", tool_count: 1, at: "2026-06-27T00:01:30Z" },
      },
    });
    expect(s.backgroundAgents[0]!.status).toBe("stalled");
    expect(s.backgroundAgents[0]!.endedAt).toBe("2026-06-27T00:01:30Z");
    s = applyEvent(s, {
      session_id: "s-1",
      seq: 3,
      event: {
        BackgroundAgentProgress: { agent_id: "a1", status: "running", tool_count: 2, at: "2026-06-27T00:01:35Z" },
      },
    });
    expect(s.backgroundAgents[0]!.status).toBe("running");
    expect(s.backgroundAgents[0]!.endedAt).toBeNull();
  });

  it("does not reopen a completed agent on a late progress event", () => {
    let s = emptyAcpState();
    s = applyEvent(s, {
      session_id: "s-1",
      seq: 1,
      event: {
        BackgroundAgentLaunched: {
          agent_id: "a1",
          tool_call_id: "task-1",
          description: "x",
          prompt: "y",
          model: "m",
          started_at: "2026-06-27T00:00:00Z",
        },
      },
    });
    s = applyEvent(s, {
      session_id: "s-1",
      seq: 2,
      event: {
        BackgroundAgentCompleted: {
          agent_id: "a1",
          status: "completed",
          ended_at: "2026-06-27T00:00:10Z",
        },
      },
    });
    s = applyEvent(s, {
      session_id: "s-1",
      seq: 3,
      event: {
        BackgroundAgentProgress: {
          agent_id: "a1",
          status: "running",
          tool_count: 99,
          at: "2026-06-27T00:00:20Z",
        },
      },
    });
    expect(s.backgroundAgents[0]!.status).toBe("completed");
    expect(s.backgroundAgents[0]!.toolCount).toBe(0);
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
    // A later replay page folds in isolation and synthesizes a sparse start
    // (kind "other", empty args) for a tool whose real start is already loaded.
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
    const next = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "hi", prompt_id: "cmp-1" } },
    });
    expect(next.promptSeq).toBe(1);
    expect(next.turnActive).toBe(true);
    // The transcript row is server-owned; applyEvent adds none.
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
    const next = applyEvent(seeded, {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "hi", prompt_id: "cmp-1" } },
    });
    expect(next.promptSeq).toBe(1);
    expect(next.inflightPromptIds).toEqual([]);
  });
});

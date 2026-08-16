import { describe, expect, it } from "vitest";

import {
  appendElicitationAnswerRow,
  applyEvent,
  emptyAcpState,
  mergeServerRows,
  patchServerRow,
  summarizeAnswers,
  transcriptRowToActivity,
  type AcpFrame,
  type AcpState,
  type Elicitation,
  type ToolCall,
} from "./acpTypes";

// Targets the AcpEvent variants and helper branches the canonical
// acpTypes.test.ts leaves cold: PlanUpdated, ThinkingEnded, the
// approval pair, ModeChanged / ModesAvailable, PromptCapabilities,
// PromptRejected, and the elicitation control path. The transcript
// (activity) fold itself is server-owned now (Tier 4), so its behavior is
// covered by the Rust `TranscriptModel` tests, not here; these assert only
// the client-side CONTROL reducer.

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

describe("applyEvent / seq gate + PlanUpdated", () => {
  it("drops frames whose seq is not greater than lastSeq (returns the same ref)", () => {
    const seeded: AcpState = { ...emptyAcpState(), lastSeq: 5 };
    const out = applyEvent(seeded, {
      session_id: "s-1",
      seq: 5,
      event: { PlanUpdated: { plan: { plan_id: "p", version: 1, steps: [] } } },
    });
    expect(out).toBe(seeded);
  });

  it("stores the plan on PlanUpdated", () => {
    const next = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        PlanUpdated: {
          plan: {
            plan_id: "plan-1",
            version: 2,
            steps: [{ id: "st-1", title: "Do it", status: "InProgress" }],
          },
        },
      },
    });
    expect(next.plan?.plan_id).toBe("plan-1");
    expect(next.plan?.steps).toHaveLength(1);
  });
});

describe("applyEvent / ThinkingEnded", () => {
  it("clears the thinking flag", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: "ThinkingStarted",
    });
    expect(state.thinking).toBe(true);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "ThinkingEnded",
    });
    expect(state.thinking).toBe(false);
  });
});

describe("applyEvent / approval lifecycle", () => {
  it("appends a pending approval on ApprovalRequested and removes it on ApprovalResolved", () => {
    const approval = {
      nonce: "ap-1",
      tool_call: tc("tc-1"),
      destructive: true,
      requested_at: "2026-01-01T00:00:00Z",
    };
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ApprovalRequested: { approval } },
    });
    expect(state.pendingApprovals).toHaveLength(1);
    expect(state.pendingApprovals[0].nonce).toBe("ap-1");

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { ApprovalResolved: { nonce: "ap-1", decision: "Allow" } },
    });
    expect(state.pendingApprovals).toHaveLength(0);
  });

  it("leaves unrelated approvals intact when a non-matching nonce resolves", () => {
    const approval = {
      nonce: "keep",
      tool_call: tc("tc-1"),
      destructive: false,
      requested_at: "2026-01-01T00:00:00Z",
    };
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ApprovalRequested: { approval } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { ApprovalResolved: { nonce: "other", decision: "Deny" } },
    });
    expect(state.pendingApprovals.map((a) => a.nonce)).toEqual(["keep"]);
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

describe("applyEvent / elicitation answer (#2209)", () => {
  const elicitation = {
    nonce: "el-1",
    message: "Pick",
    questions: [],
    requested_at: "2026-01-01T00:00:00Z",
  };

  it("clears the pending card on ElicitationResolved without touching the server-owned transcript", () => {
    // The answered row is now a server transcript row (Tier 4), so applyEvent
    // only drops the pending card; it must never mutate activity.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ElicitationRequested: { elicitation } },
    });
    expect(state.pendingElicitations).toHaveLength(1);

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        ElicitationResolved: {
          nonce: "el-1",
          outcome: "Accepted",
          answers: [{ question: "Proceed?", answer: "Yes" }],
        },
      },
    });
    expect(state.pendingElicitations).toHaveLength(0);
    expect(state.activity).toHaveLength(0);
  });
});

describe("applyEvent / mode events", () => {
  it("ModeChanged updates the legacy mode enum", () => {
    const next = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ModeChanged: { mode: "Plan" } },
    });
    expect(next.mode).toBe("Plan");
  });

  it("ModesAvailable populates the advertised modes and current id, normalising missing descriptions", () => {
    const next = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        ModesAvailable: {
          current_mode_id: "m2",
          modes: [
            { id: "m1", name: "Default", description: "the default" },
            { id: "m2", name: "Plan" },
          ],
        },
      },
    });
    expect(next.currentModeId).toBe("m2");
    expect(next.availableModes).toEqual([
      { id: "m1", name: "Default", description: "the default" },
      { id: "m2", name: "Plan", description: null },
    ]);
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

describe("applyEvent / suppressed elicitation completion clears inFlight", () => {
  it("nulls inFlightTool when the suppressed AskUserQuestion call completes", () => {
    const elicitation = {
      nonce: "e-1",
      message: "Pick",
      tool_call_id: "tc-ask",
      questions: [],
      requested_at: "2026-01-01T00:00:00Z",
      resolved: null,
    };
    // Tool starts first (in-flight pointer set), then the elicitation
    // arrives and strips the row but keeps the in-flight pointer cleared,
    // then a completion lands. Re-arm in-flight to a different tool to
    // assert the completion only clears it when it matches.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ToolCallStarted: { tool_call: tc("tc-ask", { name: "Asking" }) } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { ElicitationRequested: { elicitation } },
    });
    // ElicitationRequested already cleared inFlightTool; re-point it at the
    // suppressed id so the completion arm exercises the matching clear.
    state = { ...state, inFlightTool: tc("tc-ask", { name: "Asking" }) };
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { ToolCallCompleted: { tool_call_id: "tc-ask", is_error: false, content: "x" } },
    });
    expect(state.inFlightTool).toBeNull();
    // No transcript card materialised for the suppressed id.
    expect(state.activity.some((r) => r.toolCallId === "tc-ask")).toBe(false);
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

describe("applyEvent / UserPromptSent turn counter (Tier 4)", () => {
  it("bumps pendingUserPromptSeq for a prompt with no matching optimistic overlay", () => {
    const next = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "hi", prompt_id: "cmp-1" } },
    });
    expect(next.pendingUserPromptSeq).toBe(1);
    expect(next.turnActive).toBe(true);
    // The transcript row is server-owned; applyEvent adds none.
    expect(next.activity).toHaveLength(0);
  });

  it("does NOT double-bump when the echoed prompt_id matches an optimistic overlay row", () => {
    const seeded: AcpState = {
      ...emptyAcpState(),
      optimisticRows: [{ id: "cmp-1", kind: "user_prompt", text: "hi", at: "t" }],
      pendingUserPromptSeq: 1,
      turnActive: true,
    };
    const next = applyEvent(seeded, {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "hi", prompt_id: "cmp-1" } },
    });
    expect(next.pendingUserPromptSeq).toBe(1);
  });
});

import { afterEach, describe, expect, it } from "vitest";

import { applyEvent, emptyAcpState, setActivityLimit, type AcpFrame, type AcpState, type ToolCall } from "./acpTypes";

// Targets the AcpEvent variants and helper branches the canonical
// acpTypes.test.ts leaves cold: PlanUpdated, ThinkingEnded, the
// approval pair, DiffEmitted, ModeChanged / ModesAvailable,
// PromptCapabilities, UserPromptSent attachment mapping, PromptRejected,
// the suppressed-elicitation completion clear, sweepOpenToolCalls, the
// activity-limit cap, and the mergeToolStart timestamp branches.

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

afterEach(() => {
  // The activity cap is module-level state; reset to unlimited so other
  // suites are unaffected by the cap test below.
  setActivityLimit(0);
});

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

describe("applyEvent / DiffEmitted", () => {
  it("appends diffs and caps the buffer at 16 most-recent", () => {
    let state = emptyAcpState();
    for (let i = 0; i < 20; i++) {
      state = applyEvent(state, {
        session_id: "s-1",
        seq: i + 1,
        event: {
          DiffEmitted: {
            diff: { path: `f${i}.rs`, created_at: "2026-01-01T00:00:00Z" },
          },
        },
      });
    }
    expect(state.recentDiffs).toHaveLength(16);
    // Oldest four dropped; first retained is f4.
    expect(state.recentDiffs[0].path).toBe("f4.rs");
    expect(state.recentDiffs[15].path).toBe("f19.rs");
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
    });
  });
});

describe("applyEvent / UserPromptSent attachments", () => {
  it("maps server attachment refs to replay-backed AcpAttachments on the no-optimistic path", () => {
    const next = applyEvent(emptyAcpState(), {
      session_id: "sess id/weird",
      seq: 3,
      event: {
        UserPromptSent: {
          text: "look at this",
          attachments: [{ id: "att 1", kind: "image", mime_type: "image/png", name: "shot.png", size: 1234 }],
        },
      },
    });
    const row = next.activity[0];
    expect(row.kind).toBe("user_prompt");
    expect(row.attachments).toHaveLength(1);
    const att = row.attachments![0];
    expect(att.id).toBe("att 1");
    expect(att.mimeType).toBe("image/png");
    expect(att.size).toBe(1234);
    // session_id and attachment id are URL-encoded into the replay path.
    expect(att.url).toBe("/api/sessions/sess%20id%2Fweird/acp/attachments/att%201");
  });

  it("adopts server attachment refs when promoting an optimistic row that had none", () => {
    const optimistic: AcpState = {
      ...emptyAcpState(),
      activity: [
        {
          id: "user-local-1",
          kind: "user_prompt",
          text: "with file",
          at: new Date().toISOString(),
        },
      ],
      pendingUserPromptSeq: 1,
      turnActive: true,
    };
    const next = applyEvent(optimistic, {
      session_id: "s-1",
      seq: 4,
      event: {
        UserPromptSent: {
          text: "with file",
          attachments: [{ id: "a1", kind: "audio", mime_type: "audio/wav", size: 9 }],
        },
      },
    });
    expect(next.activity).toHaveLength(1);
    expect(next.activity[0].id).toBe("user-seq-4");
    expect(next.activity[0].attachments).toHaveLength(1);
    expect(next.activity[0].attachments![0].kind).toBe("audio");
    // Optimistic match must not bump the prompt counter again.
    expect(next.pendingUserPromptSeq).toBe(1);
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

describe("sweepOpenToolCalls via Stopped", () => {
  it("synthesizes a tool_stopped row for an open tool, draining its buffered output", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "run" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { ToolCallStarted: { tool_call: tc("open-1", { name: "LongTask" }) } },
    });
    // Buffer some streamed output that never got a completion.
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { ToolCallContent: { tool_call_id: "open-1", content: "partial out" } },
    });
    expect(state.toolOutputs["open-1"]).toBe("partial out");

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 4,
      event: { Stopped: { reason: "prompt_complete" } },
    });
    const stopped = state.activity.find((r) => r.kind === "tool_stopped" && r.toolCallId === "open-1");
    expect(stopped).toBeDefined();
    expect(stopped!.text).toBe("partial out");
    expect(stopped!.id).toBe("stopped-open-1-4");
    // Buffer drained so a replay can't double-render it.
    expect(state.toolOutputs["open-1"]).toBeUndefined();
    expect(state.inFlightTool).toBeNull();
  });

  it("does not sweep a tool that already has a terminal completion row", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "run" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { ToolCallStarted: { tool_call: tc("done-1") } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { ToolCallCompleted: { tool_call_id: "done-1", is_error: false, content: "ok" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 4,
      event: { Stopped: { reason: "prompt_complete" } },
    });
    expect(state.activity.some((r) => r.kind === "tool_stopped" && r.toolCallId === "done-1")).toBe(false);
  });
});

describe("pushActivity respects the configured activity cap", () => {
  it("retains only the most recent N rows when a cap is set", () => {
    setActivityLimit(3);
    let state = emptyAcpState();
    for (let i = 1; i <= 6; i++) {
      state = applyEvent(state, {
        session_id: "s-1",
        seq: i,
        event: { AgentMessageChunk: { text: `chunk ${i}` } },
      });
    }
    expect(state.activity).toHaveLength(3);
    expect(state.activity.map((r) => r.text)).toEqual(["chunk 4", "chunk 5", "chunk 6"]);
  });
});

describe("applyEvent / pass-through and non-matching update rows", () => {
  it("passes RawAgentUpdate and TodoListUpdated through with only seq advanced", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { RawAgentUpdate: { payload: { whatever: true } } },
    });
    expect(state.lastSeq).toBe(1);
    expect(state.activity).toEqual([]);

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { TodoListUpdated: { todos: [{ id: "t1", text: "x", completed: false }] } },
    });
    expect(state.lastSeq).toBe(2);
    expect(state.activity).toEqual([]);
  });

  it("ToolCallUpdated leaves non-matching tool_start rows untouched while patching the target", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ToolCallStarted: { tool_call: tc("other", { name: "Other", args_preview: '{"k":1}' }) } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { ToolCallStarted: { tool_call: tc("target", { name: "Target" }) } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { ToolCallUpdated: { tool_call_id: "target", title: "Renamed", args_preview: '{"k":2}' } },
    });
    const other = state.activity.find((r) => r.kind === "tool_start" && r.toolCallId === "other");
    const target = state.activity.find((r) => r.kind === "tool_start" && r.toolCallId === "target");
    // The non-matching row is returned unchanged by the map.
    expect(other?.tool?.name).toBe("Other");
    expect(other?.tool?.args_preview).toBe('{"k":1}');
    // The matching row is patched.
    expect(target?.tool?.name).toBe("Renamed");
    expect(target?.tool?.args_preview).toBe('{"k":2}');
  });
});

describe("mergeToolStart timestamp branches", () => {
  it("keeps the earlier started_at when the duplicate start carries an older timestamp", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        ToolCallStarted: {
          tool_call: tc("dup", { started_at: "2026-01-01T00:00:10Z", args_preview: '{"x":1}' }),
        },
      },
    });
    // Second start has an EARLIER started_at; the existing later one wins.
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        ToolCallStarted: {
          tool_call: tc("dup", { started_at: "2026-01-01T00:00:05Z", kind: "other", args_preview: "" }),
        },
      },
    });
    const row = state.activity.find((r) => r.kind === "tool_start" && r.toolCallId === "dup");
    expect(row?.tool?.started_at).toBe("2026-01-01T00:00:10Z");
    // Richer args/kind from the first frame survive the sparse second.
    expect(row?.tool?.args_preview).toBe('{"x":1}');
    expect(row?.tool?.kind).toBe("execute");
  });

  it("carries parent_tool_call_id and memory_recall through a merge", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ToolCallStarted: { tool_call: tc("dup2", { args_preview: "" }) } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        ToolCallStarted: {
          tool_call: tc("dup2", {
            parent_tool_call_id: "parent-9",
            memory_recall: { mode: "recall", paths: ["/a"] },
            args_preview: '{"y":2}',
          }),
        },
      },
    });
    const row = state.activity.find((r) => r.kind === "tool_start" && r.toolCallId === "dup2");
    expect(row?.tool?.parent_tool_call_id).toBe("parent-9");
    expect(row?.tool?.memory_recall?.mode).toBe("recall");
  });
});

// Reducer tests for the turn-end open-tool sweep (#1646).
//
// When the agent is stopped while a tool is mid-execution,
// claude-agent-acp resolves the prompt with StopReason::Cancelled and
// never emits a per-tool completion, so the structured view only ever sees a
// turn-level Stopped. A tool card's status is derived from the paired
// terminal row in state.activity, so without a sweep the card sticks on
// "running" forever (orange badge + a timer that counts up), live and
// on reload (the trailing Stopped is persisted and replayed through
// this same reducer).
//
// These assert that every turn-ending arm synthesizes a tool_stopped
// row for each still-open tool_start, exactly once, without disturbing
// tools that already completed.

import { describe, expect, it } from "vitest";

import { applyEvent, emptyAcpState, type AcpEvent, type AcpFrame, type AcpState, type ToolCall } from "./acpTypes";

function frame(seq: number, event: AcpEvent): AcpFrame {
  return { session_id: "s-1", seq, event };
}

function toolCall(id: string): ToolCall {
  return {
    id,
    name: `tool ${id}`,
    kind: "execute",
    args_preview: "{}",
    started_at: "2026-01-01T00:00:00.000Z",
  };
}

function start(state: AcpState, seq: number, id: string): AcpState {
  return applyEvent(state, frame(seq, { ToolCallStarted: { tool_call: toolCall(id) } }));
}

function openRows(state: AcpState): string[] {
  const terminal = new Set(
    state.activity
      .filter((r) => r.kind === "tool_complete" || r.kind === "tool_error" || r.kind === "tool_stopped")
      .map((r) => r.toolCallId),
  );
  return state.activity.filter((r) => r.kind === "tool_start" && !terminal.has(r.toolCallId)).map((r) => r.toolCallId!);
}

function stoppedRows(state: AcpState) {
  return state.activity.filter((r) => r.kind === "tool_stopped");
}

describe("turn-end open-tool sweep", () => {
  it("closes an open tool on Stopped and clears inFlightTool", () => {
    const opened = start(emptyAcpState(), 1, "t1");
    expect(openRows(opened)).toEqual(["t1"]);

    const next = applyEvent(opened, frame(2, { Stopped: { reason: "cancelled" } }));

    expect(openRows(next)).toEqual([]);
    expect(next.inFlightTool).toBeNull();
    const stopped = stoppedRows(next);
    expect(stopped).toHaveLength(1);
    expect(stopped[0]).toMatchObject({
      kind: "tool_stopped",
      toolCallId: "t1",
    });
    expect(typeof stopped[0]!.at).toBe("string");
  });

  it("leaves an already-completed tool untouched", () => {
    let state = start(emptyAcpState(), 1, "t1");
    state = applyEvent(
      state,
      frame(2, {
        ToolCallCompleted: {
          tool_call_id: "t1",
          is_error: false,
          content: "ok",
          completed_at: "2026-01-01T00:00:01.000Z",
        },
      }),
    );
    const next = applyEvent(state, frame(3, { Stopped: { reason: "cancelled" } }));

    expect(stoppedRows(next)).toHaveLength(0);
    expect(next.activity.filter((r) => r.kind === "tool_complete")).toHaveLength(1);
  });

  it("closes only the still-open tool when several are in flight", () => {
    let state = start(emptyAcpState(), 1, "a");
    state = start(state, 2, "b");
    state = applyEvent(
      state,
      frame(3, {
        ToolCallCompleted: { tool_call_id: "a", is_error: false, content: "" },
      }),
    );
    const next = applyEvent(state, frame(4, { Stopped: { reason: "user_stopped" } }));

    const stopped = stoppedRows(next);
    expect(stopped).toHaveLength(1);
    expect(stopped[0]!.toolCallId).toBe("b");
  });

  it("drains buffered ToolCallContent into the synthesized row", () => {
    let state = start(emptyAcpState(), 1, "t1");
    state = applyEvent(
      state,
      frame(2, {
        ToolCallContent: { tool_call_id: "t1", content: "partial output" },
      }),
    );
    const next = applyEvent(state, frame(3, { Stopped: { reason: "cancelled" } }));

    expect(stoppedRows(next)[0]).toMatchObject({ text: "partial output" });
    expect(next.toolOutputs.t1).toBeUndefined();
  });

  it("does not double-close on a second terminal event", () => {
    const opened = start(emptyAcpState(), 1, "t1");
    const once = applyEvent(opened, frame(2, { Stopped: { reason: "cancelled" } }));
    const twice = applyEvent(once, frame(3, { Stopped: { reason: "prompt_complete" } }));

    expect(stoppedRows(twice)).toHaveLength(1);
  });

  it("synthesizes one row even when the store carries duplicate tool_start rows", () => {
    // Pre-fix stores can replay the same tool_call_id twice; the dedupe
    // in ToolCallStarted patches in place, so seed the duplicate directly
    // to exercise the sweep's own toolCallId dedupe.
    const base = start(emptyAcpState(), 1, "dup");
    const withDupe: AcpState = {
      ...base,
      activity: base.activity.concat({
        id: "start-dup-2",
        kind: "tool_start",
        text: "tool dup",
        toolCallId: "dup",
        at: "2026-01-01T00:00:00.000Z",
      }),
    };
    const next = applyEvent(withDupe, frame(2, { Stopped: { reason: "cancelled" } }));

    expect(stoppedRows(next)).toHaveLength(1);
  });

  it("repairs a persisted [start, Stopped] replay so reload is terminal", () => {
    // Cold reload: no live state, replay the persisted frames in order.
    const replayed = [
      frame(1, { ToolCallStarted: { tool_call: toolCall("t1") } }),
      frame(2, { Stopped: { reason: "cancelled" } }),
    ].reduce(applyEvent, emptyAcpState());

    expect(openRows(replayed)).toEqual([]);
    expect(stoppedRows(replayed)).toHaveLength(1);
  });

  it.each([
    ["AgentSwitched", { AgentSwitched: { from: "claude", to: "codex", reason: "rate_limit" } }],
    ["IncompatibleAgent", { IncompatibleAgent: { detail: { reason: "x" } } }],
    ["AgentStartupError", { AgentStartupError: { message: "boom" } }],
  ] as const)("closes open tools on %s", (_label, event) => {
    const opened = start(emptyAcpState(), 1, "t1");
    const next = applyEvent(opened, frame(2, event as AcpEvent));

    expect(openRows(next)).toEqual([]);
    expect(stoppedRows(next)).toHaveLength(1);
  });
});

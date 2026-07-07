// Regression for #2711: prepending an older history page must not emit a
// duplicate `toolCallId`. When a tool call is split across the page seam
// (its ToolCallStarted in the older page, its ToolCallCompleted already in
// the loaded tail), the tail synthesized a placeholder start; the older
// page's real start must merge into it, not append a second tool_start row.
// Two assistant-ui `tool-call` parts sharing a toolCallId make useResources
// throw "Duplicate key" and crash the structured view.

import { describe, expect, it } from "vitest";

import { reducer } from "./useAcpSession";
import { type AcpEvent, type AcpFrame, reduceFrames, type ToolCall } from "../lib/acpTypes";

const frame = (seq: number, event: AcpEvent): AcpFrame => ({ session_id: "s", seq, event });

const startFrame = (seq: number, tool: ToolCall): AcpFrame => frame(seq, { ToolCallStarted: { tool_call: tool } });

const completeFrame = (seq: number, id: string, completedAt: string): AcpFrame =>
  frame(seq, { ToolCallCompleted: { tool_call_id: id, is_error: false, content: "done", completed_at: completedAt } });

const toolStarts = (rows: { kind: string; toolCallId?: string }[], id: string) =>
  rows.filter((r) => r.kind === "tool_start" && r.toolCallId === id);

describe("prepend seam dedupe (#2711)", () => {
  it("merges the older page's real start into the tail's synthesized start", () => {
    // Tail: completion for call_X with no preceding start (start fell below
    // the recent-first window), so the reducer synthesized a placeholder.
    const tail = reduceFrames([completeFrame(10, "call_X", "2024-01-01T00:05:00Z")]);
    expect(toolStarts(tail.activity, "call_X")).toHaveLength(1);
    expect(toolStarts(tail.activity, "call_X")[0]!.toolCallId).toBe("call_X");

    // Older page carries the real ToolCallStarted for the same id.
    const real: ToolCall = {
      id: "call_X",
      name: "Read",
      kind: "read",
      args_preview: '{"path":"/etc/hosts"}',
      started_at: "2024-01-01T00:00:00Z",
    };
    const next = reducer(tail, { kind: "prepend", frames: [startFrame(5, real)], oldestSeq: 5 });

    // Exactly one tool_start row survives for call_X, carrying the real
    // tool name/kind and the real (earlier) start time, not the synth
    // placeholder or the completion timestamp.
    const merged = toolStarts(next.activity, "call_X");
    expect(merged).toHaveLength(1);
    const row = merged[0]! as { tool?: ToolCall; at?: string };
    expect(row.tool?.name).toBe("Read");
    expect(row.tool?.kind).toBe("read");
    expect(row.tool?.started_at).toBe("2024-01-01T00:00:00Z");
    expect(next.oldestSeq).toBe(5);
  });

  it("prepends a non-overlapping older start as its own row", () => {
    const tail = reduceFrames([completeFrame(10, "call_X", "2024-01-01T00:05:00Z")]);
    const other: ToolCall = {
      id: "call_Y",
      name: "Bash",
      kind: "execute",
      args_preview: "{}",
      started_at: "2024-01-01T00:00:00Z",
    };
    const next = reducer(tail, { kind: "prepend", frames: [startFrame(5, other)], oldestSeq: 5 });
    expect(toolStarts(next.activity, "call_Y")).toHaveLength(1);
    expect(toolStarts(next.activity, "call_X")).toHaveLength(1);
  });

  it("leaves a prompt-only older page untouched", () => {
    const tail = reduceFrames([frame(10, { UserPromptSent: { text: "tail" } })]);
    const next = reducer(tail, {
      kind: "prepend",
      frames: [frame(5, { UserPromptSent: { text: "older" } })],
      oldestSeq: 5,
    });
    expect(next.activity.map((r) => r.text)).toEqual(["older", "tail"]);
  });
});

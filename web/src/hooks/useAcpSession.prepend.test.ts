// Regression for #2711: prepending an older history page must not emit a
// duplicate `toolCallId`. The transcript is server-owned now (Tier 4): the
// `?view=rows` replay folds each page in isolation, so a tool call split
// across the page seam (its start in the older page, its completion in the
// loaded tail) yields a synthesized placeholder start in the tail page's
// rows; the older page's real start must merge into it, not append a second
// tool_start row. Two assistant-ui `tool-call` parts sharing a toolCallId make
// useResources throw "Duplicate key" and crash the structured view.

import { describe, expect, it } from "vitest";

import { reducer } from "./useAcpSession";
import { type ActivityRow, emptyAcpState, type ToolCall } from "../lib/acpTypes";

const tc = (over: Partial<ToolCall> & { id: string }): ToolCall => ({
  name: "Read",
  kind: "read",
  args_preview: "{}",
  started_at: "2024-01-01T00:00:00Z",
  ...over,
});

const startRow = (tool: ToolCall): ActivityRow => ({
  id: `start-${tool.id}`,
  kind: "tool_start",
  text: tool.name,
  toolCallId: tool.id,
  tool,
  at: tool.started_at,
});

const doneRow = (id: string, at: string): ActivityRow => ({
  id: `done-${id}`,
  kind: "tool_complete",
  text: "done",
  toolCallId: id,
  at,
});

const withActivity = (rows: ActivityRow[]) => ({ ...emptyAcpState(), activity: rows });

const toolStarts = (rows: ActivityRow[], id: string) =>
  rows.filter((r) => r.kind === "tool_start" && r.toolCallId === id);

describe("prepend seam dedupe (#2711)", () => {
  it("merges the older page's real start into the tail's synthesized start", () => {
    // Tail rows: a synthesized placeholder start for call_X (its real start
    // fell below the recent-first window) plus its completion.
    const synth = startRow(tc({ id: "call_X", name: "tool call", kind: "other", args_preview: "" }));
    const tail = withActivity([synth, doneRow("call_X", "2024-01-01T00:05:00Z")]);

    // Older page rows carry the real ToolCallStarted for the same id.
    const real = startRow(tc({ id: "call_X", name: "Read", kind: "read", args_preview: '{"path":"/etc/hosts"}' }));
    const next = reducer(tail, { kind: "prepend", rows: [real], oldestSeq: 5 });

    // Exactly one tool_start row survives for call_X, carrying the real
    // tool name/kind and the real (earlier) start time.
    const merged = toolStarts(next.activity, "call_X");
    expect(merged).toHaveLength(1);
    expect(merged[0]!.tool?.name).toBe("Read");
    expect(merged[0]!.tool?.kind).toBe("read");
    expect(merged[0]!.tool?.started_at).toBe("2024-01-01T00:00:00Z");
    expect(next.oldestSeq).toBe(5);
  });

  it("prepends a non-overlapping older start as its own row", () => {
    const synth = startRow(tc({ id: "call_X", name: "tool call", kind: "other" }));
    const tail = withActivity([synth, doneRow("call_X", "2024-01-01T00:05:00Z")]);
    const other = startRow(tc({ id: "call_Y", name: "Bash", kind: "execute" }));
    const next = reducer(tail, { kind: "prepend", rows: [other], oldestSeq: 5 });
    expect(toolStarts(next.activity, "call_Y")).toHaveLength(1);
    expect(toolStarts(next.activity, "call_X")).toHaveLength(1);
  });

  it("prepends a prompt-only older page ahead of the tail", () => {
    const tail = withActivity([{ id: "user-seq-10", kind: "user_prompt", text: "tail", at: "t" }]);
    const next = reducer(tail, {
      kind: "prepend",
      rows: [{ id: "user-seq-5", kind: "user_prompt", text: "older", at: "t" }],
      oldestSeq: 5,
    });
    expect(next.activity.map((r) => r.text)).toEqual(["older", "tail"]);
  });
});

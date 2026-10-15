import { describe, expect, it } from "vitest";

import type { ActivityRow } from "./acpTypes";
import {
  DEFAULT_HISTORY_WINDOW,
  historyWindow,
  historyWindowStart,
  initialHistoryWindow,
  lastUserBoundaryIndex,
} from "./acpHistoryWindow";

function row(kind: ActivityRow["kind"], i: number): ActivityRow {
  return { id: `${kind}-${i}`, kind, text: `${kind} ${i}` };
}

function transcript(turns: number, perTurn: number): ActivityRow[] {
  const rows: ActivityRow[] = [];
  for (let t = 0; t < turns; t += 1) {
    rows.push(row("user_prompt", t));
    for (let r = 0; r < perTurn; r += 1) rows.push(row("message", t * 100 + r));
  }
  return rows;
}

function toolRow(id: string, parentId?: string): ActivityRow {
  return {
    id,
    kind: "tool_start",
    text: id,
    tool: {
      id,
      name: id,
      kind: "other",
      args_preview: "{}",
      started_at: "",
      parent_tool_call_id: parentId,
    },
  };
}

function subagentTranscript(lead: number, children: number, parentChain: string[] = ["task1"]): ActivityRow[] {
  const rows: ActivityRow[] = [row("user_prompt", 0)];
  for (let i = 0; i < lead; i += 1) rows.push(row("tool_complete", i));
  for (let p = 0; p < parentChain.length; p += 1) {
    rows.push(toolRow(parentChain[p]!, p === 0 ? undefined : parentChain[p - 1]!));
  }
  const leafParent = parentChain[parentChain.length - 1]!;
  for (let c = 0; c < children; c += 1) rows.push(toolRow(`child-${c}`, leafParent));
  return rows;
}

describe("historyWindowStart", () => {
  const hugeTurn = () => [row("user_prompt", 0), ...Array.from({ length: 500 }, (_, i) => row("tool_complete", i))];
  const diffTurn = () => {
    const rows = Array.from({ length: 40 }, (_, i) => row("message", i));
    rows[35] = row("user_diff_comments", 35);
    return rows;
  };

  it.each<[string, () => ActivityRow[], number, number]>([
    ["everything fits", () => transcript(2, 3), DEFAULT_HISTORY_WINDOW, 0],
    ["exactly fits", () => transcript(2, 3), 8, 0],
    ["the window exceeds the transcript", () => transcript(5, 5), 1000, 0],
    ["a zero window", () => transcript(10, 10), 0, 0],
    ["a negative window", () => transcript(10, 10), -5, 0],
    ["snaps forward to a user turn", () => transcript(10, 10), 30, 88],
    ["hard-cuts one huge turn", hugeTurn, 150, 351],
    ["treats diff comments as a boundary", diffTurn, 10, 35],
    ["pulls back to a Task parent (#2313)", () => subagentTranscript(100, 50), 40, 101],
    ["keeps a cut on the Task parent", () => subagentTranscript(100, 50), 51, 101],
    ["walks a nested parent chain", () => subagentTranscript(100, 49, ["task1", "task2"]), 40, 101],
  ])("%s", (_name, rows, visible, expected) => {
    expect(historyWindowStart(rows(), visible)).toBe(expected);
  });
});

describe("historyWindow", () => {
  it("suppresses load-earlier when the only hidden rows are pre-clear", () => {
    const rows: ActivityRow[] = [];
    for (let t = 0; t < 100; t += 1) {
      rows.push(row("user_prompt", t));
      rows.push(row("message", t));
    }
    rows.push(row("session_cleared", 999));
    for (let t = 0; t < 2; t += 1) {
      rows.push(row("user_prompt", 1000 + t));
      rows.push(row("message", 1000 + t));
    }
    const w = historyWindow(rows, DEFAULT_HISTORY_WINDOW, false);
    expect(w.start).toBeLessThan(rows.length - 5);
    expect(w.canLoadEarlier).toBe(false);
  });

  it("can load earlier post-clear rows, and ignores the clear when cleared turns are shown", () => {
    const rows: ActivityRow[] = [row("session_cleared", 0)];
    for (let i = 0; i < 200; i += 1) rows.push(row("message", i));
    expect(historyWindow(rows, 30, false).canLoadEarlier).toBe(true);
    expect(historyWindow(rows, 30, true).canLoadEarlier).toBe(true);
  });
});

describe("initialHistoryWindow", () => {
  it("keeps the default when the last turn fits inside it", () => {
    const rows = transcript(100, 1); // 200 rows, last turn = 2 rows
    expect(initialHistoryWindow(rows)).toBe(DEFAULT_HISTORY_WINDOW);
  });

  it("widens to the whole last turn when that turn alone is longer than the default", () => {
    const rows = transcript(3, 2);
    const promptIdx = rows.length;
    rows.push(row("user_prompt", 99));
    for (let i = 0; i < 400; i += 1) rows.push(row("tool_complete", i));
    expect(lastUserBoundaryIndex(rows)).toBe(promptIdx);
    const visible = initialHistoryWindow(rows);
    expect(visible).toBe(401);
    expect(historyWindowStart(rows, visible)).toBe(promptIdx);
  });

  it("counts typed diff comments as the last turn's boundary", () => {
    const rows: ActivityRow[] = transcript(2, 1);
    rows.push(row("user_diff_comments", 7));
    for (let i = 0; i < 200; i += 1) rows.push(row("message", i));
    expect(initialHistoryWindow(rows)).toBe(201);
  });

  it("falls back to the default when the transcript has no user turn", () => {
    const rows: ActivityRow[] = [];
    for (let i = 0; i < 500; i += 1) rows.push(row("message", i));
    expect(lastUserBoundaryIndex(rows)).toBe(-1);
    expect(initialHistoryWindow(rows)).toBe(DEFAULT_HISTORY_WINDOW);
    expect(initialHistoryWindow([])).toBe(DEFAULT_HISTORY_WINDOW);
  });
});

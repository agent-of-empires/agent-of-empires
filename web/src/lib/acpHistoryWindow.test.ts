import { describe, expect, it } from "vitest";

import type { ActivityRow } from "./acpTypes";
import { DEFAULT_HISTORY_WINDOW, historyWindow, historyWindowStart } from "./acpHistoryWindow";

function row(kind: ActivityRow["kind"], i: number): ActivityRow {
  return { id: `${kind}-${i}`, kind, text: `${kind} ${i}` };
}

/** A transcript of `turns` turns, each a user_prompt followed by
 *  `perTurn` assistant/tool rows. */
function transcript(turns: number, perTurn: number): ActivityRow[] {
  const rows: ActivityRow[] = [];
  for (let t = 0; t < turns; t += 1) {
    rows.push(row("user_prompt", t));
    for (let r = 0; r < perTurn; r += 1) rows.push(row("message", t * 100 + r));
  }
  return rows;
}

describe("historyWindowStart", () => {
  it("returns 0 when everything fits", () => {
    const rows = transcript(2, 3); // 8 rows
    expect(historyWindowStart(rows, DEFAULT_HISTORY_WINDOW)).toBe(0);
    expect(historyWindowStart(rows, 8)).toBe(0);
  });

  it("snaps the cap cut forward to the nearest user turn boundary", () => {
    // 10 turns x 10 rows = 110 rows, prompts at 0,11,22,...,99.
    const rows = transcript(10, 10);
    // visibleRows 30 -> cap = 80. Next boundary at index 88 (turn 8).
    const start = historyWindowStart(rows, 30);
    expect(rows[start]!.kind).toBe("user_prompt");
    expect(start).toBe(88);
    // Never renders MORE than the cap allows.
    expect(rows.length - start).toBeLessThanOrEqual(30);
  });

  it("hard-cuts at the cap when one huge turn has no boundary after it", () => {
    // One prompt then 500 tool rows: no boundary at or after the cap.
    const rows: ActivityRow[] = [row("user_prompt", 0)];
    for (let i = 0; i < 500; i += 1) rows.push(row("tool_complete", i));
    const start = historyWindowStart(rows, 150);
    expect(start).toBe(rows.length - 150); // 351
    expect(rows.length - start).toBe(150);
  });

  it("counts user_diff_comments as a turn boundary", () => {
    const rows: ActivityRow[] = [];
    for (let i = 0; i < 40; i += 1) rows.push(row("message", i));
    rows[35] = row("user_diff_comments", 35);
    // cap = 40 - 10 = 30; first boundary at or after 30 is index 35.
    expect(historyWindowStart(rows, 10)).toBe(35);
  });

  it("walks down to 0 as the window grows past the transcript", () => {
    const rows = transcript(5, 5); // 30 rows
    expect(historyWindowStart(rows, 30)).toBe(0);
    expect(historyWindowStart(rows, 1000)).toBe(0);
  });

  it("treats a non-positive window as show-all", () => {
    const rows = transcript(10, 10);
    expect(historyWindowStart(rows, 0)).toBe(0);
    expect(historyWindowStart(rows, -5)).toBe(0);
  });
});

describe("historyWindow", () => {
  it("can load earlier when rows are windowed out and there is no clear", () => {
    const rows = transcript(10, 10); // 110 rows
    const w = historyWindow(rows, 30, false);
    expect(w.start).toBeGreaterThan(0);
    expect(w.canLoadEarlier).toBe(true);
  });

  it("cannot load earlier when everything fits", () => {
    const rows = transcript(2, 3); // 8 rows
    expect(historyWindow(rows, DEFAULT_HISTORY_WINDOW, false)).toEqual({ start: 0, canLoadEarlier: false });
  });

  it("suppresses load-earlier when the only hidden rows are pre-clear", () => {
    // 100 pre-clear turns, a clear, then 2 short post-clear turns.
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
    // The window starts well before the clear, but those rows are folded
    // behind the banner, so the control must stay hidden.
    expect(w.start).toBeLessThan(rows.length - 5);
    expect(w.canLoadEarlier).toBe(false);
  });

  it("can load earlier post-clear rows, and ignores the clear when cleared turns are shown", () => {
    const rows: ActivityRow[] = [row("session_cleared", 0)];
    for (let i = 0; i < 200; i += 1) rows.push(row("message", i));
    // Folding on: hidden rows are post-clear, so load-earlier is offered.
    expect(historyWindow(rows, 30, false).canLoadEarlier).toBe(true);
    // Folding off (showClearedTurns): clear is ignored, gate is start > 0.
    expect(historyWindow(rows, 30, true).canLoadEarlier).toBe(true);
  });
});

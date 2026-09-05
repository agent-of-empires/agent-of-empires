// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import { renderHook, act } from "@testing-library/react";

import type { ActivityRow } from "../../lib/acpTypes";
import { DEFAULT_HISTORY_WINDOW } from "../../lib/acpHistoryWindow";
import { useHistoryWindow } from "../useHistoryWindow";

function transcript(turns: number, perTurn: number): ActivityRow[] {
  const rows: ActivityRow[] = [];
  for (let t = 0; t < turns; t += 1) {
    rows.push({ id: `u-${t}`, kind: "user_prompt", text: `prompt ${t}` });
    for (let r = 0; r < perTurn; r += 1) rows.push({ id: `m-${t}-${r}`, kind: "message", text: `msg ${t}.${r}` });
  }
  return rows;
}

describe("useHistoryWindow", () => {
  it("windows a long transcript and offers Load earlier", () => {
    const activity = transcript(100, 1); // 200 rows
    const { result } = renderHook(() => useHistoryWindow("s1", activity, false));
    expect(result.current.windowedActivity.length).toBeLessThanOrEqual(DEFAULT_HISTORY_WINDOW);
    expect(result.current.windowedActivity.length).toBeLessThan(activity.length);
    expect(result.current.canLoadEarlier).toBe(true);
  });

  it("renders everything and hides the control for a short transcript", () => {
    const activity = transcript(3, 1); // 6 rows
    const { result } = renderHook(() => useHistoryWindow("s1", activity, false));
    expect(result.current.windowedActivity).toHaveLength(activity.length);
    expect(result.current.canLoadEarlier).toBe(false);
  });

  it("loadEarlier grows the window until the whole transcript shows", () => {
    const activity = transcript(100, 1); // 200 rows
    const { result } = renderHook(() => useHistoryWindow("s1", activity, false));
    for (let i = 0; i < 5 && result.current.canLoadEarlier; i += 1) {
      act(() => result.current.loadEarlier());
    }
    expect(result.current.windowedActivity).toHaveLength(activity.length);
    expect(result.current.canLoadEarlier).toBe(false);
  });

  it("keeps earlier rows on screen when new turns append (no re-fold)", () => {
    // Regression for #2236 symptom A: an end-anchored window slid its
    // start forward on every appended row, folding visible older rows
    // back behind "Load earlier".
    const activity = transcript(100, 1); // 200 rows
    const { result, rerender } = renderHook(({ a }) => useHistoryWindow("s1", a, false), {
      initialProps: { a: activity },
    });
    const topBefore = result.current.windowedActivity[0]!.id;
    expect(topBefore).toBeDefined();
    // Stream 5 more turns (10 rows) onto the tail.
    const appended = activity.concat(
      Array.from({ length: 5 }, (_, t) => [
        { id: `nu-${t}`, kind: "user_prompt" as const, text: `new ${t}` },
        { id: `nm-${t}`, kind: "message" as const, text: `reply ${t}` },
      ]).flat(),
    );
    rerender({ a: appended });
    const ids = result.current.windowedActivity.map((r) => r.id);
    // The row the user could see is still rendered, and the new turns
    // landed too.
    expect(ids).toContain(topBefore);
    expect(ids).toContain("nu-4");
  });

  it("keeps a cut inside a long turn in place when the next prompt lands (#3707)", () => {
    // One assistant turn longer than the window: the cut lands mid-turn with
    // no user boundary at or after it.
    const long: ActivityRow[] = [{ id: "u-0", kind: "user_prompt", text: "big question" }];
    for (let r = 0; r < DEFAULT_HISTORY_WINDOW + 10; r += 1) {
      long.push({ id: `m-0-${r}`, kind: "message", text: `part ${r}` });
    }
    const { result, rerender } = renderHook(({ a }) => useHistoryWindow("s1", a, false), {
      initialProps: { a: long },
    });
    const topBefore = result.current.windowedActivity[0]!.id;
    expect(topBefore).toMatch(/^m-0-/);
    expect(result.current.canLoadEarlier).toBe(true);

    // The next prompt used to become the nearest boundary the start snapped
    // forward to, dropping every rendered part of the long turn at once.
    const next = long.concat([
      { id: "u-1", kind: "user_prompt", text: "follow-up" },
      { id: "m-1-0", kind: "message", text: "short answer" },
    ]);
    rerender({ a: next });
    expect(result.current.windowedActivity[0]!.id).toBe(topBefore);
    expect(result.current.windowedActivity.at(-1)!.id).toBe("m-1-0");
    expect(result.current.canLoadEarlier).toBe(true);

    // Load earlier still moves the start back, and the pin follows it.
    act(() => result.current.loadEarlier());
    expect(result.current.windowedActivity[0]!.id).toBe("u-0");
  });

  it("drops the pin when its row is trimmed away", () => {
    const activity = transcript(100, 1);
    const { result, rerender } = renderHook(({ a }) => useHistoryWindow("s1", a, false), {
      initialProps: { a: activity },
    });
    const topBefore = result.current.windowedActivity[0]!.id;
    // A retention trim removes the head of the transcript, pin included.
    const trimmed = activity.filter((r) => r.id !== topBefore).slice(60);
    rerender({ a: trimmed });
    expect(result.current.windowedActivity[0]!.id).toBe(trimmed[0]!.id);
  });

  it("resets the window to recent when the session changes", () => {
    const activity = transcript(100, 1);
    const { result, rerender } = renderHook(({ id }) => useHistoryWindow(id, activity, false), {
      initialProps: { id: "s1" },
    });
    act(() => result.current.loadEarlier());
    act(() => result.current.loadEarlier());
    const grown = result.current.windowedActivity.length;
    rerender({ id: "s2" });
    expect(result.current.windowedActivity.length).toBeLessThan(grown);
    expect(result.current.canLoadEarlier).toBe(true);
  });
});

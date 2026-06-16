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

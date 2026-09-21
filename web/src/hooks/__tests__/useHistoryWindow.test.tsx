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

/** One prompt followed by `extra` rows past the default window, so the turn alone overflows it. */
function longTurn(extra: number): ActivityRow[] {
  const rows: ActivityRow[] = [{ id: "u-0", kind: "user_prompt", text: "big question" }];
  for (let r = 0; r < DEFAULT_HISTORY_WINDOW + extra; r += 1) {
    rows.push({ id: `m-0-${r}`, kind: "message", text: `part ${r}` });
  }
  return rows;
}

const followUp: ActivityRow[] = [
  { id: "u-1", kind: "user_prompt", text: "follow-up" },
  { id: "m-1-0", kind: "message", text: "short answer" },
];

const render = (a: ActivityRow[], sid = "s1") =>
  renderHook(({ sid, a }: { sid: string; a: ActivityRow[] }) => useHistoryWindow(sid, a, false), {
    initialProps: { sid, a },
  });

describe("useHistoryWindow", () => {
  it("windows a long transcript and offers Load earlier", () => {
    const activity = transcript(100, 1); // 200 rows
    const { result } = render(activity);
    expect(result.current.windowedActivity.length).toBeLessThanOrEqual(DEFAULT_HISTORY_WINDOW);
    expect(result.current.windowedActivity.length).toBeLessThan(activity.length);
    expect(result.current.canLoadEarlier).toBe(true);
  });

  it("renders everything and hides the control for a short transcript", () => {
    const activity = transcript(3, 1); // 6 rows
    const { result } = render(activity);
    expect(result.current.windowedActivity).toHaveLength(activity.length);
    expect(result.current.canLoadEarlier).toBe(false);
  });

  it("loadEarlier grows the window until the whole transcript shows", () => {
    const activity = transcript(100, 1);
    const { result } = render(activity);
    for (let i = 0; i < 5 && result.current.canLoadEarlier; i += 1) {
      act(() => result.current.loadEarlier());
    }
    expect(result.current.windowedActivity).toHaveLength(activity.length);
    expect(result.current.canLoadEarlier).toBe(false);
  });

  it("keeps earlier rows on screen when new turns append (no re-fold)", () => {
    const activity = transcript(100, 1);
    const { result, rerender } = render(activity);
    const topBefore = result.current.windowedActivity[0]!.id;
    expect(topBefore).toBeDefined();
    const appended = activity.concat(
      Array.from({ length: 5 }, (_, t) => [
        { id: `nu-${t}`, kind: "user_prompt" as const, text: `new ${t}` },
        { id: `nm-${t}`, kind: "message" as const, text: `reply ${t}` },
      ]).flat(),
    );
    rerender({ sid: "s1", a: appended });
    const ids = result.current.windowedActivity.map((r) => r.id);
    expect(ids).toContain(topBefore);
    expect(ids).toContain("nu-4");
  });

  it("never snaps a long last turn forward when the next prompt lands (#3707)", () => {
    const long = longTurn(10);
    const { result, rerender } = render(long);
    expect(result.current.windowedActivity[0]!.id).toBe("u-0");
    expect(result.current.windowedActivity).toHaveLength(long.length);
    expect(result.current.canLoadEarlier).toBe(false);

    rerender({ sid: "s1", a: long.concat(followUp) });
    expect(result.current.windowedActivity[0]!.id).toBe("u-0");
    expect(result.current.windowedActivity.at(-1)!.id).toBe("m-1-0");
    expect(result.current.canLoadEarlier).toBe(false);
  });

  it("holds a mid-turn cut when a prompt lands after a no-boundary open (pinnedWindowStart)", () => {
    const noBoundary: ActivityRow[] = [];
    for (let r = 0; r < DEFAULT_HISTORY_WINDOW + 100; r += 1) {
      noBoundary.push({ id: `m-${r}`, kind: "message", text: `part ${r}` });
    }
    const { result, rerender } = render(noBoundary);
    expect(result.current.windowedActivity[0]!.id).toBe("m-100");
    expect(result.current.canLoadEarlier).toBe(true);

    rerender({ sid: "s1", a: noBoundary.concat(followUp) });
    expect(result.current.windowedActivity[0]!.id).toBe("m-100");
    expect(result.current.windowedActivity.at(-1)!.id).toBe("m-1-0");
  });

  it("drops the pin when its row is trimmed away", () => {
    const activity = transcript(100, 1);
    const { result, rerender } = render(activity);
    const topBefore = result.current.windowedActivity[0]!.id;
    const trimmed = activity.filter((r) => r.id !== topBefore).slice(60);
    rerender({ sid: "s1", a: trimmed });
    expect(result.current.windowedActivity[0]!.id).toBe(trimmed[0]!.id);
  });

  it("resets the window to recent when the session changes", () => {
    const activity = transcript(100, 1);
    const { result, rerender } = render(activity);
    act(() => result.current.loadEarlier());
    act(() => result.current.loadEarlier());
    const grown = result.current.windowedActivity.length;
    rerender({ sid: "s2", a: activity });
    expect(result.current.windowedActivity.length).toBeLessThan(grown);
    expect(result.current.canLoadEarlier).toBe(true);
  });

  it("opens on the whole last turn when that turn is longer than the default window", () => {
    const activity = transcript(5, 1); // 10 rows
    activity.push({ id: "u-last", kind: "user_prompt", text: "the last prompt" });
    for (let r = 0; r < DEFAULT_HISTORY_WINDOW + 200; r += 1) {
      activity.push({ id: `t-${r}`, kind: "tool_complete", text: `tool ${r}` });
    }
    const { result } = render(activity);
    const ids = result.current.windowedActivity.map((r) => r.id);
    expect(ids[0]).toBe("u-last");
    expect(ids).toHaveLength(DEFAULT_HISTORY_WINDOW + 201);
    expect(result.current.canLoadEarlier).toBe(true);
  });

  it("re-sizes to the new session's last turn on a session switch", () => {
    const { result, rerender } = render(transcript(100, 1)); // last turn is 2 rows: default window
    expect(result.current.windowedActivity.length).toBeLessThanOrEqual(DEFAULT_HISTORY_WINDOW);

    const long = longTurn(50);
    rerender({ sid: "s2", a: long });
    expect(result.current.windowedActivity[0]!.id).toBe("u-0");
    expect(result.current.windowedActivity).toHaveLength(long.length);
    expect(result.current.canLoadEarlier).toBe(false);
  });
});

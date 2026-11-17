// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";
import { renderHook, act } from "@testing-library/react";

import { useQueuedCounts } from "../useAcpQueueCount";
import { STORAGE_KEY_PREFIX, getLiveQueueCount, setLiveQueueCount } from "../../lib/acpStateStorage";
import type { SessionResponse } from "../../lib/types";

const row = (id: string, queued_count?: number) => ({ id, queued_count }) as SessionResponse;

const render = (initial: SessionResponse[]) =>
  renderHook(({ sessions }) => useQueuedCounts(sessions), { initialProps: { sessions: initial } });

afterEach(() => {
  localStorage.clear();
  for (const id of ["a", "b"]) setLiveQueueCount(id, null);
});

describe("useQueuedCounts", () => {
  it("takes the Daemon's count for a session this browser never opened, whatever is persisted", () => {
    const stale = {
      savedAt: Date.now(),
      state: { lastSeq: 0, activity: [], queuedPrompts: [{ id: "x" }, { id: "y" }] },
    };
    localStorage.setItem(`${STORAGE_KEY_PREFIX}a`, JSON.stringify(stale));
    const { result } = render([row("a", 1), row("b")]);
    expect(result.current).toEqual([1, 0]);
  });

  it("takes this browser's count while it holds the live queue, and the Daemon's again once it doesn't", () => {
    const { result } = render([row("a", 1), row("b", 3)]);
    // Live, with an optimistic row the Daemon hasn't confirmed yet.
    act(() => setLiveQueueCount("a", 2));
    expect(result.current).toEqual([2, 3]);
    // The connection closes: nothing stays stuck at the live value.
    act(() => setLiveQueueCount("a", null));
    expect(result.current).toEqual([1, 3]);
    // A reconnect that lists the queue again is live again.
    act(() => setLiveQueueCount("a", 0));
    expect(result.current).toEqual([0, 3]);
  });

  it("follows a new Daemon count from the next poll", () => {
    const { result, rerender } = render([row("a", 1)]);
    rerender({ sessions: [row("a", 4)] });
    expect(result.current).toEqual([4]);
  });

  it("re-renders only for its own sessions and stops listening on unmount", () => {
    let renders = 0;
    const { unmount } = renderHook(() => {
      renders += 1;
      return useQueuedCounts([row("a", 1)]);
    });
    const before = renders;
    act(() => setLiveQueueCount("b", 5));
    expect(renders).toBe(before);
    unmount();
    act(() => setLiveQueueCount("a", 2));
    expect(getLiveQueueCount("a")).toBe(2);
    expect(renders).toBe(before);
  });

  it("is empty for a row with no sessions", () => {
    expect(render([]).result.current).toEqual([]);
  });
});

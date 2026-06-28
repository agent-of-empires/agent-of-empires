// @vitest-environment jsdom
//
// Coverage for useConversationSearch: debounces, skips queries below the
// minimum length, and drops a stale in-flight response when the query
// changes so out-of-order resolution never shows old results.

import { renderHook, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useConversationSearch } from "./useConversationSearch";
import * as api from "../lib/api";

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("useConversationSearch", () => {
  it("does not search below the minimum length", () => {
    const spy = vi.spyOn(api, "searchConversations").mockResolvedValue([]);
    renderHook(() => useConversationSearch("a"));
    act(() => vi.advanceTimersByTime(500));
    expect(spy).not.toHaveBeenCalled();
  });

  it("debounces then returns results", async () => {
    const spy = vi
      .spyOn(api, "searchConversations")
      .mockResolvedValue([{ session_id: "s1", seq: 3, kind: "agent", snippet: "hit", match_count: 1 }]);
    const { result } = renderHook(() => useConversationSearch("reconciler"));
    expect(spy).not.toHaveBeenCalled(); // still inside the debounce window
    await act(async () => {
      vi.advanceTimersByTime(250);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(spy).toHaveBeenCalledOnce();
    expect(result.current.results).toHaveLength(1);
  });

  it("drops a stale response when the query changes", async () => {
    // First query resolves slowly; second resolves immediately. The hook
    // must show only the second query's results.
    let resolveFirst: (v: api.ConversationSearchHit[]) => void = () => {};
    const spy = vi
      .spyOn(api, "searchConversations")
      .mockImplementationOnce(() => new Promise((res) => (resolveFirst = res)))
      .mockResolvedValueOnce([{ session_id: "s2", seq: 1, kind: "user", snippet: "second", match_count: 1 }]);

    const { result, rerender } = renderHook(({ q }) => useConversationSearch(q), {
      initialProps: { q: "first" },
    });
    await act(async () => {
      vi.advanceTimersByTime(250);
      await Promise.resolve();
    });

    rerender({ q: "second" });
    await act(async () => {
      vi.advanceTimersByTime(250);
      await Promise.resolve();
    });

    // Late first response arrives after the query changed; it was aborted,
    // so applying it must be a no-op.
    await act(async () => {
      resolveFirst([{ session_id: "s1", seq: 9, kind: "agent", snippet: "first", match_count: 1 }]);
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(spy).toHaveBeenCalledTimes(2);
    expect(result.current.results).toEqual([
      { session_id: "s2", seq: 1, kind: "user", snippet: "second", match_count: 1 },
    ]);
  });
});

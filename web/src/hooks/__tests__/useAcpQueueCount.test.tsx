// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { renderHook, act } from "@testing-library/react";

import { useQueuedCountForSessions } from "../useAcpQueueCount";
import {
  STORAGE_KEY_PREFIX,
  clearQueueCount,
  setQueueCount,
} from "../../lib/acpStateStorage";

function entryKey(id: string): string {
  return `${STORAGE_KEY_PREFIX}${id}`;
}

// Write a persisted acp-state entry shaped like useAcpSession's
// persistState output, with `queuedPrompts` of the given length.
function writeEntry(id: string, queued: number, savedAt = Date.now()): void {
  const queuedPrompts = Array.from({ length: queued }, (_, i) => ({
    id: `${id}-${i}`,
    text: `q${i}`,
    queuedAt: new Date(savedAt).toISOString(),
  }));
  localStorage.setItem(
    entryKey(id),
    JSON.stringify({
      savedAt,
      state: { lastSeq: 0, activity: [], queuedPrompts },
    }),
  );
}

beforeEach(() => {
  localStorage.clear();
  // Reset the module-level in-memory count cache between cases so a
  // prior test's writes don't leak into the next.
  clearQueueCount();
});

afterEach(() => {
  localStorage.clear();
  clearQueueCount();
});

describe("useQueuedCountForSessions", () => {
  it("returns 0 when no session has queued prompts", () => {
    const { result } = renderHook(() => useQueuedCountForSessions(["a"]));
    expect(result.current).toBe(0);
  });

  it("lazily reads the count from a persisted localStorage entry", () => {
    writeEntry("a", 3);
    const { result } = renderHook(() => useQueuedCountForSessions(["a"]));
    expect(result.current).toBe(3);
  });

  it("sums queued counts across the workspace's sessions", () => {
    writeEntry("a", 2);
    writeEntry("b", 1);
    const { result } = renderHook(() => useQueuedCountForSessions(["a", "b"]));
    expect(result.current).toBe(3);
  });

  // Story: queued prompts drain (Stopped pops the head) or the queue is
  // cleared; the badge updates to the remaining count and disappears at 0.
  it("updates same-tab as the queue grows and drains via persistState", () => {
    const { result } = renderHook(() => useQueuedCountForSessions(["a"]));
    expect(result.current).toBe(0);

    act(() => setQueueCount("a", 2));
    expect(result.current).toBe(2);

    act(() => setQueueCount("a", 1));
    expect(result.current).toBe(1);

    act(() => setQueueCount("a", 0));
    expect(result.current).toBe(0);
  });

  // Story: one tab enqueues a prompt; another tab's sidebar updates via
  // the `storage` event without polling.
  it("updates cross-tab via a storage event without a local write", () => {
    const { result } = renderHook(() => useQueuedCountForSessions(["a"]));
    expect(result.current).toBe(0);

    const newValue = JSON.stringify({
      savedAt: Date.now(),
      state: {
        lastSeq: 0,
        activity: [],
        queuedPrompts: [
          { id: "a-0", text: "q0", queuedAt: "t" },
          { id: "a-1", text: "q1", queuedAt: "t" },
        ],
      },
    });
    act(() => {
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: entryKey("a"),
          newValue,
          storageArea: localStorage,
        }),
      );
    });
    expect(result.current).toBe(2);
  });

  it("does not react to a storage event for an unsubscribed session", () => {
    const { result } = renderHook(() => useQueuedCountForSessions(["a"]));
    act(() => {
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: entryKey("b"),
          newValue: JSON.stringify({
            savedAt: Date.now(),
            state: {
              lastSeq: 0,
              activity: [],
              queuedPrompts: [{ id: "b-0", text: "x", queuedAt: "t" }],
            },
          }),
          storageArea: localStorage,
        }),
      );
    });
    expect(result.current).toBe(0);
  });

  it("treats a TTL-expired entry as 0", () => {
    const eightDaysAgo = Date.now() - 8 * 24 * 60 * 60 * 1000;
    writeEntry("a", 5, eightDaysAgo);
    const { result } = renderHook(() => useQueuedCountForSessions(["a"]));
    expect(result.current).toBe(0);
  });

  it("treats a corrupt entry as 0", () => {
    localStorage.setItem(entryKey("a"), "{not json");
    const { result } = renderHook(() => useQueuedCountForSessions(["a"]));
    expect(result.current).toBe(0);
  });

  it("removes its storage listener on unmount", () => {
    const { unmount, result } = renderHook(() =>
      useQueuedCountForSessions(["a"]),
    );
    unmount();
    // After unmount the listener is gone, so a cross-tab event must not
    // throw or leave a dangling subscriber. The hook value is frozen at
    // its last render.
    act(() => {
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: entryKey("a"),
          newValue: JSON.stringify({
            savedAt: Date.now(),
            state: {
              lastSeq: 0,
              activity: [],
              queuedPrompts: [{ id: "a-0", text: "x", queuedAt: "t" }],
            },
          }),
          storageArea: localStorage,
        }),
      );
    });
    expect(result.current).toBe(0);
  });
});

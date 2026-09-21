// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { renderHook, act } from "@testing-library/react";

import { useQueuedCountForSessions } from "../useAcpQueueCount";
import { STORAGE_KEY_PREFIX, clearQueueCount, getQueuedCount, setQueueCount } from "../../lib/acpStateStorage";

function entry(id: string, queued: number, savedAt = Date.now()): string {
  const queuedPrompts = Array.from({ length: queued }, (_, i) => ({ id: `${id}-${i}`, text: `q${i}`, queuedAt: "t" }));
  return JSON.stringify({ savedAt, state: { lastSeq: 0, activity: [], queuedPrompts } });
}

function dispatchStorage(id: string, queued: number): void {
  act(() => {
    window.dispatchEvent(
      new StorageEvent("storage", {
        key: `${STORAGE_KEY_PREFIX}${id}`,
        newValue: entry(id, queued),
        storageArea: localStorage,
      }),
    );
  });
}

const render = (ids: string[]) => renderHook(() => useQueuedCountForSessions(ids));

beforeEach(() => {
  localStorage.clear();
  clearQueueCount();
});

afterEach(() => {
  localStorage.clear();
  clearQueueCount();
});

describe("useQueuedCountForSessions", () => {
  it.each([
    ["no entries", {}, 0],
    ["one persisted entry", { a: entry("a", 3) }, 3],
    ["the sum across sessions", { a: entry("a", 2), b: entry("b", 1) }, 3],
    ["a TTL-expired entry", { a: entry("a", 5, Date.now() - 8 * 24 * 60 * 60 * 1000) }, 0],
    ["a corrupt entry", { a: "{not json" }, 0],
  ] as [string, Record<string, string>, number][])("reads %s lazily from storage", (_label, stored, expected) => {
    for (const [id, raw] of Object.entries(stored)) localStorage.setItem(`${STORAGE_KEY_PREFIX}${id}`, raw);
    expect(render(["a", "b"]).result.current).toBe(expected);
  });

  it("follows same-tab writes", () => {
    const { result } = render(["a"]);
    for (const n of [2, 1, 0]) {
      act(() => setQueueCount("a", n));
      expect(result.current).toBe(n);
    }
  });

  it("follows cross-tab storage events for subscribed sessions only", () => {
    const { result } = render(["a"]);
    dispatchStorage("b", 1);
    expect(result.current).toBe(0);
    dispatchStorage("a", 2);
    expect(result.current).toBe(2);
  });

  it("removes its storage listener on unmount", () => {
    const { unmount } = render(["a"]);
    act(() => setQueueCount("a", 2));
    unmount();
    dispatchStorage("a", 1);
    expect(getQueuedCount("a")).toBe(2);
  });
});

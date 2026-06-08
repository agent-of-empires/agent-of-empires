// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  STORAGE_KEY_PREFIX,
  STATE_TTL_MS,
  clearQueueCount,
  getQueuedCount,
  setQueueCount,
  subscribeAcpState,
} from "./acpStateStorage";

function entryKey(id: string): string {
  return `${STORAGE_KEY_PREFIX}${id}`;
}

function writeEntry(id: string, queued: number, savedAt = Date.now()): void {
  const queuedPrompts = Array.from({ length: queued }, (_, i) => ({
    id: `${id}-${i}`,
    text: `q${i}`,
    queuedAt: "t",
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
  clearQueueCount();
});

afterEach(() => {
  localStorage.clear();
  clearQueueCount();
  vi.restoreAllMocks();
});

describe("getQueuedCount", () => {
  it("returns 0 for a missing entry", () => {
    expect(getQueuedCount("nope")).toBe(0);
  });

  it("parses the count from localStorage on a cache miss", () => {
    writeEntry("a", 4);
    expect(getQueuedCount("a")).toBe(4);
  });

  it("memoises the count so a later localStorage edit is not re-read", () => {
    writeEntry("a", 2);
    expect(getQueuedCount("a")).toBe(2);
    // Direct localStorage mutation does not invalidate the in-memory
    // cache; only setQueueCount / a storage event / clear do.
    writeEntry("a", 9);
    expect(getQueuedCount("a")).toBe(2);
  });

  it("treats a TTL-expired entry as 0", () => {
    writeEntry("a", 5, Date.now() - STATE_TTL_MS - 1);
    expect(getQueuedCount("a")).toBe(0);
  });

  it("treats a corrupt entry as 0", () => {
    localStorage.setItem(entryKey("a"), "{not json");
    expect(getQueuedCount("a")).toBe(0);
  });

  it("treats an entry without a queuedPrompts array as 0", () => {
    localStorage.setItem(
      entryKey("a"),
      JSON.stringify({ savedAt: Date.now(), state: { lastSeq: 0 } }),
    );
    expect(getQueuedCount("a")).toBe(0);
  });

  it("returns 0 when localStorage access throws", () => {
    const spy = vi
      .spyOn(Storage.prototype, "getItem")
      .mockImplementation(() => {
        throw new Error("blocked");
      });
    expect(getQueuedCount("a")).toBe(0);
    spy.mockRestore();
  });
});

describe("setQueueCount", () => {
  it("publishes the count and notifies subscribers", () => {
    const cb = vi.fn();
    const unsub = subscribeAcpState(cb, new Set(["a"]));
    setQueueCount("a", 3);
    expect(cb).toHaveBeenCalledTimes(1);
    expect(getQueuedCount("a")).toBe(3);
    unsub();
  });

  it("does not notify a subscriber filtered to other sessions", () => {
    const cb = vi.fn();
    const unsub = subscribeAcpState(cb, new Set(["b"]));
    setQueueCount("a", 1);
    expect(cb).not.toHaveBeenCalled();
    unsub();
  });
});

describe("clearQueueCount", () => {
  it("clears one session and notifies that session's subscribers", () => {
    setQueueCount("a", 2);
    const cb = vi.fn();
    const unsub = subscribeAcpState(cb, new Set(["a"]));
    clearQueueCount("a");
    expect(cb).toHaveBeenCalledTimes(1);
    // Cache dropped; with no localStorage entry the count reads 0.
    expect(getQueuedCount("a")).toBe(0);
    unsub();
  });

  it("clears all sessions and notifies every subscriber", () => {
    const cbA = vi.fn();
    const cbB = vi.fn();
    const unsubA = subscribeAcpState(cbA, new Set(["a"]));
    const unsubB = subscribeAcpState(cbB, new Set(["b"]));
    clearQueueCount();
    expect(cbA).toHaveBeenCalledTimes(1);
    expect(cbB).toHaveBeenCalledTimes(1);
    unsubA();
    unsubB();
  });
});

describe("subscribeAcpState storage events", () => {
  it("refreshes the cache and notifies on a matching cross-tab write", () => {
    const cb = vi.fn();
    const unsub = subscribeAcpState(cb, new Set(["a"]));
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
    expect(cb).toHaveBeenCalledTimes(1);
    expect(getQueuedCount("a")).toBe(1);
    unsub();
  });

  it("ignores keys outside the acp-state prefix", () => {
    const cb = vi.fn();
    const unsub = subscribeAcpState(cb, new Set(["a"]));
    window.dispatchEvent(
      new StorageEvent("storage", {
        key: "some:other:key",
        newValue: "x",
        storageArea: localStorage,
      }),
    );
    expect(cb).not.toHaveBeenCalled();
    unsub();
  });

  it("treats a removed entry (null newValue) as 0", () => {
    setQueueCount("a", 3);
    const cb = vi.fn();
    const unsub = subscribeAcpState(cb, new Set(["a"]));
    window.dispatchEvent(
      new StorageEvent("storage", {
        key: entryKey("a"),
        newValue: null,
        storageArea: localStorage,
      }),
    );
    expect(cb).toHaveBeenCalledTimes(1);
    expect(getQueuedCount("a")).toBe(0);
    unsub();
  });

  it("fires unconditionally and drops the cache on a cross-tab clear (null key)", () => {
    setQueueCount("a", 2);
    const cb = vi.fn();
    const unsub = subscribeAcpState(cb, new Set(["a"]));
    window.dispatchEvent(
      new StorageEvent("storage", { key: null, storageArea: localStorage }),
    );
    expect(cb).toHaveBeenCalledTimes(1);
    expect(getQueuedCount("a")).toBe(0);
    unsub();
  });

  it("a null-filter listener fires for any session change", () => {
    const cb = vi.fn();
    const unsub = subscribeAcpState(cb, null);
    setQueueCount("whatever", 1);
    expect(cb).toHaveBeenCalledTimes(1);
    unsub();
  });

  it("stops firing after unsubscribe", () => {
    const cb = vi.fn();
    const unsub = subscribeAcpState(cb, new Set(["a"]));
    unsub();
    setQueueCount("a", 1);
    window.dispatchEvent(
      new StorageEvent("storage", {
        key: entryKey("a"),
        newValue: null,
        storageArea: localStorage,
      }),
    );
    expect(cb).not.toHaveBeenCalled();
  });
});

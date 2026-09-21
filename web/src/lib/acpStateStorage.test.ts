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

const entryKey = (id: string) => `${STORAGE_KEY_PREFIX}${id}`;
const entry = (queued: number, savedAt = Date.now()) =>
  JSON.stringify({
    savedAt,
    state: {
      lastSeq: 0,
      activity: [],
      queuedPrompts: Array.from({ length: queued }, (_, i) => ({ id: `q${i}`, text: `q${i}`, queuedAt: "t" })),
    },
  });

const unsubs: (() => void)[] = [];
function listen(filter: string[] | null) {
  const cb = vi.fn();
  unsubs.push(subscribeAcpState(cb, filter && new Set(filter)));
  return cb;
}
const storageEvent = (key: string | null, newValue: string | null = null) =>
  window.dispatchEvent(new StorageEvent("storage", { key, newValue, storageArea: localStorage }));

beforeEach(() => {
  localStorage.clear();
  clearQueueCount();
});

afterEach(() => {
  for (const unsub of unsubs.splice(0)) unsub();
  localStorage.clear();
  clearQueueCount();
  vi.restoreAllMocks();
});

describe("getQueuedCount", () => {
  it.each<[string, string | null, number]>([
    ["a missing entry", null, 0],
    ["a stored entry", entry(4), 4],
    ["a TTL-expired entry", entry(5, Date.now() - STATE_TTL_MS - 1), 0],
    ["a corrupt entry", "{not json", 0],
    ["an entry without queuedPrompts", JSON.stringify({ savedAt: Date.now(), state: { lastSeq: 0 } }), 0],
  ])("reads %s", (_name, raw, expected) => {
    if (raw !== null) localStorage.setItem(entryKey("a"), raw);
    expect(getQueuedCount("a")).toBe(expected);
  });

  it("memoises the parsed count", () => {
    localStorage.setItem(entryKey("a"), entry(2));
    expect(getQueuedCount("a")).toBe(2);
    localStorage.setItem(entryKey("a"), entry(9));
    expect(getQueuedCount("a")).toBe(2);
  });

  it("returns 0 when localStorage access throws", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(getQueuedCount("a")).toBe(0);
  });
});

describe("publishing and subscriptions", () => {
  it("setQueueCount publishes and notifies only matching subscribers", () => {
    const a = listen(["a"]);
    const b = listen(["b"]);
    const all = listen(null);
    setQueueCount("a", 3);
    expect(getQueuedCount("a")).toBe(3);
    expect([a, b, all].map((cb) => cb.mock.calls.length)).toEqual([1, 0, 1]);
  });

  it("clearQueueCount clears one session or all of them", () => {
    setQueueCount("a", 2);
    const a = listen(["a"]);
    const b = listen(["b"]);
    clearQueueCount("a");
    expect(getQueuedCount("a")).toBe(0);
    expect([a, b].map((cb) => cb.mock.calls.length)).toEqual([1, 0]);
    clearQueueCount();
    expect([a, b].map((cb) => cb.mock.calls.length)).toEqual([2, 1]);
  });

  it.each<[string, string | null, string | null, number, number]>([
    ["a matching cross-tab write", entryKey("a"), entry(1), 1, 1],
    ["a removed entry", entryKey("a"), null, 1, 0],
    ["an unrelated key", "some:other:key", "x", 0, 3],
    ["a cross-tab clear", null, null, 1, 0],
  ])("storage event for %s", (_name, key, value, calls, count) => {
    setQueueCount("a", 3);
    const cb = listen(["a"]);
    storageEvent(key, value);
    expect(cb).toHaveBeenCalledTimes(calls);
    expect(getQueuedCount("a")).toBe(count);
  });

  it("stops firing after unsubscribe", () => {
    const cb = listen(["a"]);
    for (const unsub of unsubs.splice(0)) unsub();
    setQueueCount("a", 1);
    storageEvent(entryKey("a"));
    expect(cb).not.toHaveBeenCalled();
  });
});

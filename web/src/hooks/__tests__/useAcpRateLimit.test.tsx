// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { renderHook, act } from "@testing-library/react";

import { useRateLimitedForSessions } from "../useAcpRateLimit";
import {
  STORAGE_KEY_PREFIX,
  clearQueueCount,
  setRateLimit,
} from "../../lib/acpStateStorage";
import type { RateLimitInfo } from "../../lib/acpTypes";

function entryKey(id: string): string {
  return `${STORAGE_KEY_PREFIX}${id}`;
}

function rl(resetsAt: string): RateLimitInfo {
  return { status: "rate_limited", resets_at: resetsAt, kind: "requests" };
}

// Write a persisted acp-state entry shaped like useAcpSession's
// persistState output, with the given rateLimit (or null).
function writeEntry(
  id: string,
  rateLimit: RateLimitInfo | null,
  savedAt = Date.now(),
): void {
  localStorage.setItem(
    entryKey(id),
    JSON.stringify({
      savedAt,
      state: { lastSeq: 0, activity: [], queuedPrompts: [], rateLimit },
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
});

describe("useRateLimitedForSessions", () => {
  it("returns null when no session is rate-limited", () => {
    const { result } = renderHook(() => useRateLimitedForSessions(["a"]));
    expect(result.current).toBeNull();
  });

  it("lazily reads rate-limit info from a persisted localStorage entry", () => {
    writeEntry("a", rl("2026-06-01T12:00:00Z"));
    const { result } = renderHook(() => useRateLimitedForSessions(["a"]));
    expect(result.current).toEqual({
      count: 1,
      resetsAt: "2026-06-01T12:00:00Z",
    });
  });

  // Story 3: only rate-limited sessions count; the soonest reset wins.
  it("counts rate-limited sessions and reports the soonest reset", () => {
    writeEntry("a", rl("2026-06-01T13:00:00Z"));
    writeEntry("b", null);
    writeEntry("c", rl("2026-06-01T12:30:00Z"));
    const { result } = renderHook(() =>
      useRateLimitedForSessions(["a", "b", "c"]),
    );
    expect(result.current).toEqual({
      count: 2,
      resetsAt: "2026-06-01T12:30:00Z",
    });
  });

  // Story 2: the session recovers (agent switch sets rateLimit null); the
  // indicator clears on the next persistState write.
  it("updates same-tab as rate-limit sets and clears via persistState", () => {
    const { result } = renderHook(() => useRateLimitedForSessions(["a"]));
    expect(result.current).toBeNull();

    act(() => setRateLimit("a", rl("2026-06-01T12:00:00Z")));
    expect(result.current).toEqual({
      count: 1,
      resetsAt: "2026-06-01T12:00:00Z",
    });

    act(() => setRateLimit("a", null));
    expect(result.current).toBeNull();
  });

  // Story 1 cross-tab: another tab persists a rate-limit; this tab's
  // sidebar updates via the storage event without a local write.
  it("updates cross-tab via a storage event without a local write", () => {
    const { result } = renderHook(() => useRateLimitedForSessions(["a"]));
    expect(result.current).toBeNull();

    const newValue = JSON.stringify({
      savedAt: Date.now(),
      state: {
        lastSeq: 0,
        activity: [],
        queuedPrompts: [],
        rateLimit: rl("2026-06-01T12:00:00Z"),
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
    expect(result.current).toEqual({
      count: 1,
      resetsAt: "2026-06-01T12:00:00Z",
    });
  });

  it("does not react to a storage event for an unsubscribed session", () => {
    const { result } = renderHook(() => useRateLimitedForSessions(["a"]));
    act(() => {
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: entryKey("b"),
          newValue: JSON.stringify({
            savedAt: Date.now(),
            state: {
              lastSeq: 0,
              activity: [],
              queuedPrompts: [],
              rateLimit: rl("2026-06-01T12:00:00Z"),
            },
          }),
          storageArea: localStorage,
        }),
      );
    });
    expect(result.current).toBeNull();
  });

  it("treats a TTL-expired entry as not rate-limited", () => {
    const eightDaysAgo = Date.now() - 8 * 24 * 60 * 60 * 1000;
    writeEntry("a", rl("2026-06-01T12:00:00Z"), eightDaysAgo);
    const { result } = renderHook(() => useRateLimitedForSessions(["a"]));
    expect(result.current).toBeNull();
  });

  it("treats a corrupt entry as not rate-limited", () => {
    localStorage.setItem(entryKey("a"), "{not json");
    const { result } = renderHook(() => useRateLimitedForSessions(["a"]));
    expect(result.current).toBeNull();
  });

  it("removes its storage listener on unmount", () => {
    const { unmount, result } = renderHook(() =>
      useRateLimitedForSessions(["a"]),
    );
    unmount();
    act(() => {
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: entryKey("a"),
          newValue: JSON.stringify({
            savedAt: Date.now(),
            state: {
              lastSeq: 0,
              activity: [],
              queuedPrompts: [],
              rateLimit: rl("2026-06-01T12:00:00Z"),
            },
          }),
          storageArea: localStorage,
        }),
      );
    });
    expect(result.current).toBeNull();
  });
});

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  IDLE_DECAY_WINDOW_MS,
  getStatusDotClass,
  getStatusTextClass,
  idleAgeMs,
  isFreshIdle,
  isSessionActive,
} from "./session";
import type { SessionResponse, SessionStatus } from "./types";

const NOW = Date.parse("2026-05-01T12:00:00Z");
/** Explicit window for tests that exercise the freshness path. The
 *  module default is 0 (off), so tests opt in by passing this explicitly. */
const TEST_WINDOW_MS = 20 * 60 * 1000;

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date(NOW));
});

afterEach(() => {
  vi.useRealTimers();
});

function session(
  status: SessionStatus,
  idleEnteredAt: string | null,
): Pick<SessionResponse, "status" | "idle_entered_at"> {
  return { status, idle_entered_at: idleEnteredAt };
}

describe("IDLE_DECAY_WINDOW_MS default", () => {
  it("is 0 (off) by default — opt-in feature", () => {
    // Guards against an accidental flip back to a non-zero default. The
    // freshness signal needs to stay opt-in across the dashboard since
    // the rattle pulses are visually noisy in steady-state usage.
    expect(IDLE_DECAY_WINDOW_MS).toBe(0);
  });
});

describe("idleAgeMs", () => {
  it("returns null for non-Idle sessions", () => {
    expect(
      idleAgeMs(session("Running", new Date(NOW - 1000).toISOString())),
    ).toBeNull();
  });

  it("returns null when idle_entered_at is missing", () => {
    expect(idleAgeMs(session("Idle", null))).toBeNull();
  });

  it("returns null when idle_entered_at is unparseable", () => {
    expect(idleAgeMs(session("Idle", "not-a-date"))).toBeNull();
  });

  it("returns null for future timestamps (clock skew)", () => {
    // Clock skew between server and browser must not look like a fresh idle.
    expect(
      idleAgeMs(session("Idle", new Date(NOW + 60_000).toISOString())),
    ).toBeNull();
  });

  it("returns elapsed milliseconds for past Idle transition", () => {
    expect(
      idleAgeMs(session("Idle", new Date(NOW - 5_000).toISOString())),
    ).toBe(5_000);
  });
});

describe("isFreshIdle", () => {
  it("is false by default (window is 0)", () => {
    // Default-window call: even a session that just transitioned should
    // be treated as not-fresh, because the freshness signal is opt-in.
    expect(
      isFreshIdle(session("Idle", new Date(NOW - 1_000).toISOString())),
    ).toBe(false);
  });

  it("is true within the explicit window", () => {
    expect(
      isFreshIdle(
        session("Idle", new Date(NOW - 60_000).toISOString()),
        TEST_WINDOW_MS,
      ),
    ).toBe(true);
  });

  it("is false past the explicit window", () => {
    expect(
      isFreshIdle(
        session("Idle", new Date(NOW - TEST_WINDOW_MS - 1).toISOString()),
        TEST_WINDOW_MS,
      ),
    ).toBe(false);
  });

  it("is false for non-Idle sessions even with a recent timestamp", () => {
    expect(
      isFreshIdle(
        session("Running", new Date(NOW - 1_000).toISOString()),
        TEST_WINDOW_MS,
      ),
    ).toBe(false);
  });

  it("is false when window is non-positive", () => {
    // Defensive: negative or zero window short-circuits before any
    // timestamp math, so a positive `idle_entered_at` can't sneak in.
    expect(
      isFreshIdle(session("Idle", new Date(NOW - 1).toISOString()), 0),
    ).toBe(false);
    expect(
      isFreshIdle(session("Idle", new Date(NOW - 1).toISOString()), -1),
    ).toBe(false);
  });
});

describe("getStatusDotClass", () => {
  it("uses idle class by default (freshness opt-in)", () => {
    // No explicit window → falls back to the off default → idle class.
    expect(
      getStatusDotClass(session("Idle", new Date(NOW - 1_000).toISOString())),
    ).toBe("bg-status-idle");
  });

  it("uses fresh-idle class when explicitly within window", () => {
    expect(
      getStatusDotClass(
        session("Idle", new Date(NOW - 1_000).toISOString()),
        TEST_WINDOW_MS,
      ),
    ).toBe("bg-status-fresh-idle");
  });

  it("falls back to idle class past the explicit window", () => {
    expect(
      getStatusDotClass(
        session("Idle", new Date(NOW - TEST_WINDOW_MS - 1_000).toISOString()),
        TEST_WINDOW_MS,
      ),
    ).toBe("bg-status-idle");
  });

  it("preserves non-Idle classes regardless of idle_entered_at", () => {
    expect(
      getStatusDotClass(
        session("Waiting", new Date(NOW - 1_000).toISOString()),
        TEST_WINDOW_MS,
      ),
    ).toBe("bg-status-waiting");
  });
});

describe("getStatusTextClass", () => {
  it("uses idle text class by default (freshness opt-in)", () => {
    expect(
      getStatusTextClass(session("Idle", new Date(NOW - 1_000).toISOString())),
    ).toBe("text-status-idle");
  });

  it("uses fresh-idle text class when explicitly within window", () => {
    expect(
      getStatusTextClass(
        session("Idle", new Date(NOW - 1_000).toISOString()),
        TEST_WINDOW_MS,
      ),
    ).toBe("text-status-fresh-idle");
  });

  it("falls back to idle class when idle_entered_at is missing", () => {
    expect(getStatusTextClass(session("Idle", null), TEST_WINDOW_MS)).toBe(
      "text-status-idle",
    );
  });
});

describe("isSessionActive", () => {
  it("treats Idle as inactive by default (freshness opt-in)", () => {
    expect(
      isSessionActive(session("Idle", new Date(NOW - 1_000).toISOString())),
    ).toBe(false);
  });

  it("treats fresh-idle as active when window is enabled", () => {
    expect(
      isSessionActive(
        session("Idle", new Date(NOW - 1_000).toISOString()),
        TEST_WINDOW_MS,
      ),
    ).toBe(true);
  });

  it("treats decayed Idle as inactive even with window enabled", () => {
    expect(
      isSessionActive(
        session("Idle", new Date(NOW - TEST_WINDOW_MS - 1_000).toISOString()),
        TEST_WINDOW_MS,
      ),
    ).toBe(false);
  });

  it("retains the legacy string-only API for callers without idle_entered_at", () => {
    // Some callers (legacy paths, unit tests) still pass a bare status. The
    // overload must keep classifying Running/Waiting/Starting as active.
    expect(isSessionActive("Running")).toBe(true);
    expect(isSessionActive("Idle")).toBe(false);
  });
});

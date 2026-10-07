import { it, expect, beforeEach, afterEach, vi } from "vitest";
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
const WINDOW = 20 * 60 * 1000;

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date(NOW));
});

afterEach(() => {
  vi.useRealTimers();
});

/** A session that entered its status `ago` ms before NOW (null for no timestamp). */
const session = (
  status: SessionStatus,
  ago: number | null | string,
  dormant = false,
): Pick<SessionResponse, "status" | "idle_entered_at" | "dormant"> => ({
  status,
  idle_entered_at: typeof ago === "number" ? new Date(NOW - ago).toISOString() : ago,
  dormant,
});

it("freshness is off by default", () => {
  expect(IDLE_DECAY_WINDOW_MS).toBe(0);
});

it.each<[SessionStatus, number | null | string, number | null]>([
  ["Running", 1000, null],
  ["Idle", null, null],
  ["Idle", "not-a-date", null],
  ["Idle", -60_000, null],
  ["Idle", 5_000, 5_000],
])("idleAgeMs(%s, %j) is %j", (status, ago, expected) => {
  expect(idleAgeMs(session(status, ago))).toBe(expected);
});

it.each<[SessionStatus, number, number | undefined, boolean]>([
  ["Idle", 1_000, undefined, false],
  ["Idle", 60_000, WINDOW, true],
  ["Idle", WINDOW + 1, WINDOW, false],
  ["Running", 1_000, WINDOW, false],
])("isFreshIdle(%s, %s ms ago, window %s) is %s", (status, ago, window, expected) => {
  expect(isFreshIdle(session(status, ago), window)).toBe(expected);
});

it.each<[SessionStatus, number | null, boolean, number | undefined, string]>([
  ["Idle", 1_000, false, undefined, "idle"],
  ["Idle", 1_000, false, WINDOW, "fresh-idle"],
  ["Idle", WINDOW + 1_000, false, WINDOW, "idle"],
  ["Idle", null, false, WINDOW, "idle"],
  ["Waiting", 1_000, false, WINDOW, "waiting"],
  ["Idle", 1_000, true, WINDOW, "dormant"],
  ["Stopped", null, false, undefined, "stopped"],
])("status classes for %s (%s ms ago, dormant=%s, window %s) use %s", (status, ago, dormant, window, suffix) => {
  const s = session(status, ago, dormant);
  expect(getStatusDotClass(s, window)).toBe(`bg-status-${suffix}`);
  expect(getStatusTextClass(s, window)).toBe(`text-status-${suffix}`);
});

it("isSessionActive counts fresh idle only with a window, and accepts a bare status", () => {
  expect(isSessionActive(session("Idle", 1_000))).toBe(false);
  expect(isSessionActive(session("Idle", 1_000), WINDOW)).toBe(true);
  expect(isSessionActive(session("Idle", WINDOW + 1_000), WINDOW)).toBe(false);
  expect(isSessionActive("Running")).toBe(true);
  expect(isSessionActive("Idle")).toBe(false);
});

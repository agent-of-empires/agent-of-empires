import { expect, it } from "vitest";

import { countUnreadSessions, countWaitingSessions, sessionIsUnread, sessionIsWaitingForInput } from "../session";
import type { SessionResponse } from "../types";

function session(overrides: Partial<SessionResponse>): SessionResponse {
  return { id: "s-1", status: "Idle", ...overrides } as SessionResponse;
}

it("sessionIsUnread flags an unread, settled, visible session other than the open one", () => {
  const at = "2026-01-01T00:00:00Z";
  const cases: [string, Partial<SessionResponse>, string | null, boolean][] = [
    ["unread, nothing open", { unread: true }, null, true],
    ["unread, another open", { unread: true }, "s-2", true],
    ["unread Unknown status", { unread: true, status: "Unknown" }, null, true],
    ["not unread", { unread: false }, null, false],
    ["no flag", {}, null, false],
    ["currently open", { unread: true }, "s-1", false],
    ["archived", { unread: true, archived_at: at }, null, false],
    ["snoozed", { unread: true, snoozed_until: at }, null, false],
    ["trashed", { unread: true, trashed_at: at }, null, false],
    // A new turn already running or awaiting input outranks a stale unread flag.
    ["Running", { unread: true, status: "Running" }, null, false],
    ["Waiting", { unread: true, status: "Waiting" }, null, false],
    ["Starting", { unread: true, status: "Starting" }, null, false],
  ];
  for (const [name, over, open, expected] of cases) expect(sessionIsUnread(session(over), open), name).toBe(expected);
});

it("sessionIsWaitingForInput is true only for a visible Waiting session", () => {
  const at = "2026-01-01T00:00:00Z";
  expect(sessionIsWaitingForInput(session({ status: "Waiting" }))).toBe(true);
  for (const over of [{ status: "Running" }, { archived_at: at }, { snoozed_until: at }, { trashed_at: at }] as const) {
    expect(sessionIsWaitingForInput(session({ status: "Waiting", ...over })), JSON.stringify(over)).toBe(false);
  }
});

it("counts unread (excluding the open one, zero when disabled) and waiting sessions", () => {
  const sessions = [session({ id: "a", unread: true }), session({ id: "b", unread: true }), session({ id: "c" })];
  expect(countUnreadSessions(sessions, null, true)).toBe(2);
  expect(countUnreadSessions(sessions, "a", true)).toBe(1);
  expect(countUnreadSessions(sessions, null, false)).toBe(0);
  const waiting = [
    session({ id: "a", status: "Waiting" }),
    session({ id: "b", status: "Waiting", archived_at: "2026-01-01T00:00:00Z" }),
    session({ id: "c", status: "Running" }),
  ];
  expect(countWaitingSessions(waiting)).toBe(1);
});

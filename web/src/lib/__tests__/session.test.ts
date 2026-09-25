import { describe, expect, it } from "vitest";

import { countUnreadSessions, countWaitingSessions, sessionIsUnread, sessionIsWaitingForInput } from "../session";
import type { SessionResponse } from "../types";

function session(overrides: Partial<SessionResponse>): SessionResponse {
  return { id: "s-1", status: "Idle", ...overrides } as SessionResponse;
}

describe("sessionIsUnread", () => {
  it("is true for an unread session that isn't the one currently open", () => {
    expect(sessionIsUnread(session({ unread: true }), null)).toBe(true);
    expect(sessionIsUnread(session({ id: "s-1", unread: true }), "s-2")).toBe(true);
  });

  it("is false when the session is not unread", () => {
    expect(sessionIsUnread(session({ unread: false }), null)).toBe(false);
    expect(sessionIsUnread(session({}), null)).toBe(false);
  });

  it("is false for the currently open session, even if unread", () => {
    expect(sessionIsUnread(session({ id: "s-1", unread: true }), "s-1")).toBe(false);
  });

  it("is false for an archived, snoozed, or trashed session", () => {
    expect(sessionIsUnread(session({ unread: true, archived_at: "2026-01-01T00:00:00Z" }), null)).toBe(false);
    expect(sessionIsUnread(session({ unread: true, snoozed_until: "2026-01-01T00:00:00Z" }), null)).toBe(false);
    expect(sessionIsUnread(session({ unread: true, trashed_at: "2026-01-01T00:00:00Z" }), null)).toBe(false);
  });

  it("is false for a session with a live status, even if unread: a new turn already running or awaiting input outranks a stale unread flag", () => {
    expect(sessionIsUnread(session({ unread: true, status: "Running" }), null)).toBe(false);
    expect(sessionIsUnread(session({ unread: true, status: "Waiting" }), null)).toBe(false);
    expect(sessionIsUnread(session({ unread: true, status: "Starting" }), null)).toBe(false);
  });

  it("is true for Idle or Unknown, matching the sidebar row's own unread-dot gate", () => {
    expect(sessionIsUnread(session({ unread: true, status: "Idle" }), null)).toBe(true);
    expect(sessionIsUnread(session({ unread: true, status: "Unknown" }), null)).toBe(true);
  });
});

describe("sessionIsWaitingForInput", () => {
  it("is true only for a live session with status Waiting", () => {
    expect(sessionIsWaitingForInput(session({ status: "Waiting" }))).toBe(true);
    expect(sessionIsWaitingForInput(session({ status: "Running" }))).toBe(false);
  });

  it("is false for an archived, snoozed, or trashed session", () => {
    expect(sessionIsWaitingForInput(session({ status: "Waiting", archived_at: "2026-01-01T00:00:00Z" }))).toBe(false);
    expect(sessionIsWaitingForInput(session({ status: "Waiting", snoozed_until: "2026-01-01T00:00:00Z" }))).toBe(false);
    expect(sessionIsWaitingForInput(session({ status: "Waiting", trashed_at: "2026-01-01T00:00:00Z" }))).toBe(false);
  });
});

describe("countUnreadSessions", () => {
  const sessions = [session({ id: "a", unread: true }), session({ id: "b", unread: true }), session({ id: "c" })];

  it("counts unread sessions, excluding the currently open one", () => {
    expect(countUnreadSessions(sessions, null, true)).toBe(2);
    expect(countUnreadSessions(sessions, "a", true)).toBe(1);
  });

  it("is 0 when the unread indicator is disabled, regardless of the raw flags", () => {
    expect(countUnreadSessions(sessions, null, false)).toBe(0);
  });
});

describe("countWaitingSessions", () => {
  it("counts live sessions waiting for input", () => {
    const sessions = [
      session({ id: "a", status: "Waiting" }),
      session({ id: "b", status: "Waiting", archived_at: "2026-01-01T00:00:00Z" }),
      session({ id: "c", status: "Running" }),
    ];
    expect(countWaitingSessions(sessions)).toBe(1);
  });
});

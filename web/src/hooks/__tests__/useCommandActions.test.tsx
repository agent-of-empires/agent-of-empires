// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";
import { useCommandActions, buildConversationActions } from "../useCommandActions";
import type { SessionResponse } from "../../lib/types";

type Args = Parameters<typeof useCommandActions>[0];

const actionsFor = (over: Partial<Args> = {}) => renderHook(() => useCommandActions(baseArgs(over))).result.current;
const find = (actions: ReturnType<typeof useCommandActions>, id: string) => actions.find((a) => a.id === id);

function baseArgs(overrides: Partial<Args> = {}): Args {
  return {
    sessions: [] as SessionResponse[],
    activeSessionId: null,
    activeSession: null,
    loginRequired: false,
    hasActiveSession: false,
    readOnly: false,
    onNewSession: vi.fn(),
    onNewScratch: vi.fn(),
    onSelectSession: vi.fn(),
    onJumpToAttention: vi.fn(),
    hasAttentionSession: false,
    onSessionStateAction: vi.fn(),
    onToggleDiff: vi.fn(),
    onOpenSettings: vi.fn(),
    onOpenHelp: vi.fn(),
    onOpenAbout: vi.fn(),
    onGoDashboard: vi.fn(),
    onToggleSidebar: vi.fn(),
    onLogout: vi.fn(),
    ...overrides,
  };
}

describe("useCommandActions creation and attention commands", () => {
  it("offers New scratch session right after New session, dispatching onNewScratch", () => {
    const onNewScratch = vi.fn();
    const actions = actionsFor({ onNewScratch });
    const ids = actions.map((a) => a.id);
    expect(ids.indexOf("action:new-scratch-session")).toBe(ids.indexOf("action:new-session") + 1);
    const scratch = find(actions, "action:new-scratch-session")!;
    expect(scratch).toMatchObject({ title: "New scratch session", group: "Actions" });
    expect(scratch.keywords).toContain("scratch");
    expect(scratch.shortcut).toMatch(/N$/);
    scratch.perform();
    expect(onNewScratch).toHaveBeenCalledTimes(1);
  });

  it("hides both creation commands in read-only mode", () => {
    const ids = actionsFor({ readOnly: true }).map((a) => a.id);
    expect(ids).not.toContain("action:new-session");
    expect(ids).not.toContain("action:new-scratch-session");
  });

  it("offers jump-to-attention only when something needs attention", () => {
    expect(find(actionsFor(), "action:jump-attention")).toBeUndefined();
    const onJumpToAttention = vi.fn();
    const jump = find(actionsFor({ hasAttentionSession: true, onJumpToAttention }), "action:jump-attention")!;
    expect(jump).toMatchObject({ title: "Go to next attention session", group: "Actions", shortcut: "a" });
    jump.perform();
    expect(onJumpToAttention).toHaveBeenCalledTimes(1);
  });
});

describe("buildConversationActions", () => {
  const hit = (over: Partial<import("../../lib/api").ConversationSearchHit> = {}) => ({
    session_id: "s1",
    seq: 1,
    kind: "agent",
    snippet: "matched text",
    match_count: 1,
    ...over,
  });
  const session = (over: Partial<SessionResponse> = {}) =>
    ({ id: "s1", title: "My Session", status: "idle", created_at: "2026-01-01T00:00:00Z", ...over }) as SessionResponse;

  it("maps a hit to Conversations row data carrying its session id", () => {
    const actions = buildConversationActions([hit()], [session()], null);
    expect(actions).toHaveLength(1);
    expect(actions[0]).toMatchObject({
      id: "conversation:s1",
      sessionId: "s1",
      title: "My Session",
      group: "Conversations",
      subtitle: "matched text",
    });
  });

  it("skips the active session and hits whose session is gone", () => {
    expect(buildConversationActions([hit()], [session()], "s1")).toHaveLength(0);
    expect(buildConversationActions([hit({ session_id: "ghost" })], [session()], null)).toHaveLength(0);
  });

  it("labels sunk state and a multi-match count", () => {
    const trashed = buildConversationActions(
      [hit({ match_count: 3 })],
      [session({ trashed_at: "2026-01-02T00:00:00Z" })],
      null,
    );
    expect(trashed[0]!.title).toBe("My Session · trashed");
    expect(trashed[0]!.subtitle).toBe("matched text (3 matches)");
    const snoozed = buildConversationActions([hit()], [session({ snoozed_until: "2099-01-01T00:00:00Z" })], null);
    expect(snoozed[0]!.title).toBe("My Session · snoozed");
  });
});

describe("useCommandActions: active-session triage toggles", () => {
  const active = (over: Partial<SessionResponse> = {}) =>
    ({ id: "act", title: "Alpha", status: "idle", created_at: "2026-01-01T00:00:00Z", ...over }) as SessionResponse;
  const stateIds = (over: Partial<SessionResponse>, args: Partial<Args> = {}) =>
    actionsFor({ activeSession: active(over), hasActiveSession: true, ...args })
      .map((a) => a.id)
      .filter((id) => id.startsWith("session-state:"));

  it.each<[string, Partial<SessionResponse>, string[]]>([
    ["no sunk state", {}, ["pin", "archive", "snooze", "trash"]],
    [
      "every sunk state",
      {
        pinned_at: "2026-01-02T00:00:00Z",
        archived_at: "2026-01-02T00:00:00Z",
        snoozed_until: "2099-01-01T00:00:00Z",
        trashed_at: "2026-01-02T00:00:00Z",
      },
      ["unpin", "unarchive", "unsnooze", "untrash"],
    ],
  ])("offers the toggles for %s", (_label, over, expected) => {
    expect(stateIds(over).sort()).toEqual(expected.map((a) => `session-state:${a}:act`).sort());
  });

  it("titles toggles and routes perform to onSessionStateAction", () => {
    const onSessionStateAction = vi.fn();
    const actions = actionsFor({ activeSession: active(), hasActiveSession: true, onSessionStateAction });
    expect(find(actions, "session-state:pin:act")).toMatchObject({ title: "Pin Alpha", group: "Actions" });
    find(actions, "session-state:archive:act")!.perform();
    expect(onSessionStateAction).toHaveBeenCalledWith("act", "archive");
    const trashed = actionsFor({ activeSession: active({ trashed_at: "t" }), hasActiveSession: true });
    expect(find(trashed, "session-state:untrash:act")?.title).toBe("Untrash Alpha");
  });

  it("omits the toggles in read-only mode", () => {
    expect(stateIds({}, { readOnly: true })).toEqual([]);
  });
});

// @vitest-environment jsdom
//
// Contract test for the command-palette action list (#1643). Asserts the
// "New scratch session" command is present with the right shape and dispatches
// onNewScratch, and that both creation commands are hidden in read-only mode
// (matching the sidebar / dashboard, which hide their "new" buttons rather than
// offering a command that opens a wizard the server 403s on submit).

import { describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";
import { useCommandActions, buildConversationActions } from "../useCommandActions";
import type { SessionResponse } from "../../lib/types";

type Args = Parameters<typeof useCommandActions>[0];

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

describe("useCommandActions: scratch command", () => {
  it("exposes a 'New scratch session' command", () => {
    const { result } = renderHook(() => useCommandActions(baseArgs()));
    const scratch = result.current.find((a) => a.id === "action:new-scratch-session");
    expect(scratch).toBeDefined();
    expect(scratch?.title).toBe("New scratch session");
    expect(scratch?.group).toBe("Actions");
    expect(scratch?.keywords).toContain("scratch");
    expect(scratch?.shortcut).toMatch(/N$/);
  });

  it("renders the scratch command right after 'New session'", () => {
    const { result } = renderHook(() => useCommandActions(baseArgs()));
    const ids = result.current.map((a) => a.id);
    const newSession = ids.indexOf("action:new-session");
    const scratch = ids.indexOf("action:new-scratch-session");
    expect(newSession).toBeGreaterThanOrEqual(0);
    expect(scratch).toBe(newSession + 1);
  });

  it("perform dispatches onNewScratch", () => {
    const onNewScratch = vi.fn();
    const { result } = renderHook(() => useCommandActions(baseArgs({ onNewScratch })));
    const scratch = result.current.find((a) => a.id === "action:new-scratch-session");
    scratch?.perform();
    expect(onNewScratch).toHaveBeenCalledTimes(1);
  });

  it("hides both creation commands in read-only mode", () => {
    const { result } = renderHook(() => useCommandActions(baseArgs({ readOnly: true })));
    const ids = result.current.map((a) => a.id);
    expect(ids).not.toContain("action:new-session");
    expect(ids).not.toContain("action:new-scratch-session");
  });
});

describe("useCommandActions: jump-to-attention command", () => {
  it("is absent when nothing needs attention", () => {
    const { result } = renderHook(() => useCommandActions(baseArgs({ hasAttentionSession: false })));
    expect(result.current.find((a) => a.id === "action:jump-attention")).toBeUndefined();
  });

  it("appears with the right shape and dispatches onJumpToAttention", () => {
    const onJumpToAttention = vi.fn();
    const { result } = renderHook(() => useCommandActions(baseArgs({ hasAttentionSession: true, onJumpToAttention })));
    const jump = result.current.find((a) => a.id === "action:jump-attention");
    expect(jump).toBeDefined();
    expect(jump?.title).toBe("Go to next attention session");
    expect(jump?.group).toBe("Actions");
    expect(jump?.shortcut).toBe("a");
    jump?.perform();
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
  const ids = (actions: ReturnType<typeof useCommandActions>) => actions.map((a) => a.id);

  it("offers the forward toggles when the session is in no sunk state", () => {
    const { result } = renderHook(() =>
      useCommandActions(baseArgs({ activeSession: active(), hasActiveSession: true })),
    );
    const got = ids(result.current);
    expect(got).toContain("session-state:pin:act");
    expect(got).toContain("session-state:archive:act");
    expect(got).toContain("session-state:snooze:act");
    expect(got).toContain("session-state:trash:act");
    expect(got).not.toContain("session-state:unpin:act");
    expect(got).not.toContain("session-state:unarchive:act");
    const pin = result.current.find((a) => a.id === "session-state:pin:act");
    expect(pin?.title).toBe("Pin Alpha");
    expect(pin?.group).toBe("Actions");
  });

  it("flips each toggle to its restore direction per state", () => {
    const { result } = renderHook(() =>
      useCommandActions(
        baseArgs({
          hasActiveSession: true,
          activeSession: active({
            pinned_at: "2026-01-02T00:00:00Z",
            archived_at: "2026-01-02T00:00:00Z",
            snoozed_until: "2099-01-01T00:00:00Z",
            trashed_at: "2026-01-02T00:00:00Z",
          }),
        }),
      ),
    );
    const got = ids(result.current);
    expect(got).toContain("session-state:unpin:act");
    expect(got).toContain("session-state:unarchive:act");
    expect(got).toContain("session-state:unsnooze:act");
    expect(got).toContain("session-state:untrash:act");
    expect(got).not.toContain("session-state:pin:act");
    expect(result.current.find((a) => a.id === "session-state:untrash:act")?.title).toBe("Untrash Alpha");
  });

  it("routes a toggle's perform to onSessionStateAction with the session id and action", () => {
    const onSessionStateAction = vi.fn();
    const { result } = renderHook(() =>
      useCommandActions(baseArgs({ activeSession: active(), hasActiveSession: true, onSessionStateAction })),
    );
    result.current.find((a) => a.id === "session-state:archive:act")!.perform();
    expect(onSessionStateAction).toHaveBeenCalledWith("act", "archive");
  });

  it("omits the toggles in read-only mode", () => {
    const { result } = renderHook(() =>
      useCommandActions(baseArgs({ activeSession: active(), hasActiveSession: true, readOnly: true })),
    );
    expect(ids(result.current).some((id) => id.startsWith("session-state:"))).toBe(false);
  });
});

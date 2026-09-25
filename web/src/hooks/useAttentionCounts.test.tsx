// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ReactNode } from "react";

import { useAttentionCounts } from "./useAttentionCounts";
import { UnreadIndicatorContext } from "../lib/unreadIndicator";
import type { SessionResponse } from "../lib/types";

function session(overrides: Partial<SessionResponse>): SessionResponse {
  return { id: "s-1", status: "Idle", ...overrides } as SessionResponse;
}

function withUnreadIndicator(enabled: boolean) {
  return function wrapper({ children }: { children: ReactNode }) {
    return <UnreadIndicatorContext.Provider value={enabled}>{children}</UnreadIndicatorContext.Provider>;
  };
}

describe("useAttentionCounts", () => {
  const sessions = [session({ id: "a", unread: true }), session({ id: "b", status: "Waiting" }), session({ id: "c" })];

  it("counts unread and waiting sessions when the unread indicator is on", () => {
    const { result } = renderHook(() => useAttentionCounts(sessions, null), { wrapper: withUnreadIndicator(true) });
    expect(result.current).toEqual({ unreadCount: 1, waitingCount: 1 });
  });

  it("zeroes the unread count (but not waiting) when the unread indicator setting is off", () => {
    const { result } = renderHook(() => useAttentionCounts(sessions, null), { wrapper: withUnreadIndicator(false) });
    expect(result.current).toEqual({ unreadCount: 0, waitingCount: 1 });
  });

  it("excludes the currently open session from the unread count", () => {
    const { result } = renderHook(() => useAttentionCounts(sessions, "a"), { wrapper: withUnreadIndicator(true) });
    expect(result.current.unreadCount).toBe(0);
  });

  it("defaults to the unread indicator being on when rendered without a provider", () => {
    const { result } = renderHook(() => useAttentionCounts(sessions, null));
    expect(result.current.unreadCount).toBe(1);
  });
});

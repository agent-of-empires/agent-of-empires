// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ReactNode } from "react";

import { useNeedsYouCount } from "./useNeedsYouCount";
import { UnreadIndicatorContext } from "../lib/unreadIndicator";
import type { SessionResponse } from "../lib/types";

const session = (overrides: Partial<SessionResponse>) => ({ id: "s", status: "Idle", ...overrides }) as SessionResponse;

const withUnread = (enabled: boolean) =>
  function wrapper({ children }: { children: ReactNode }) {
    return <UnreadIndicatorContext.Provider value={enabled}>{children}</UnreadIndicatorContext.Provider>;
  };

describe("useNeedsYouCount", () => {
  const sessions = [
    session({ id: "waiting", status: "Waiting" }),
    session({ id: "error", status: "Error" }),
    session({ id: "urgent", status: "Running", urgent: true }),
    session({ id: "unread", unread: true }),
    session({ id: "calm" }),
    session({ id: "snoozed", status: "Waiting", snoozed_until: "2026-01-01T00:00:00Z" }),
  ];

  it("counts Waiting, Error, urgent and, with the unread indicator on, unread Sessions, as the headers do", () => {
    for (const [enabled, count] of [
      [true, 4],
      [false, 3],
    ] as const) {
      const { result } = renderHook(() => useNeedsYouCount(sessions), { wrapper: withUnread(enabled) });
      expect(result.current, `unread ${enabled}`).toBe(count);
    }
  });
});

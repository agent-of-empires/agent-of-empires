// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";

vi.mock("../../lib/api", () => ({
  fetchSessions: vi.fn().mockResolvedValue(null),
}));

import { useSessions } from "../useSessions";
import type { SessionResponse } from "../../lib/types";

const base = (over: Partial<SessionResponse>): SessionResponse =>
  ({ id: "s1", title: "t", status: "Running", trashed_at: null, ...over }) as SessionResponse;

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.runOnlyPendingTimers();
  vi.useRealTimers();
});

describe("useSessions.applySession (#2489)", () => {
  it("replaces the matching session with the snapshot", () => {
    const { result } = renderHook(() => useSessions());
    act(() => result.current.injectSession(base({})));

    const trashed = base({ status: "Stopped", trashed_at: "2026-01-01T00:00:00Z" });
    act(() => result.current.applySession(trashed));

    expect(result.current.sessions).toHaveLength(1);
    expect(result.current.sessions[0]).toEqual(trashed);
  });

  it("is a no-op when the id is absent", () => {
    const { result } = renderHook(() => useSessions());
    act(() => result.current.injectSession(base({})));

    act(() => result.current.applySession(base({ id: "other", title: "x" })));

    expect(result.current.sessions).toHaveLength(1);
    expect(result.current.sessions[0].id).toBe("s1");
  });
});

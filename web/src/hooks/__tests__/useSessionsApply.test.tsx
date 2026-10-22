// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";

vi.mock("../../lib/api", () => ({
  fetchSessions: vi.fn().mockResolvedValue(null),
}));

import { useSessions } from "../useSessions";
import { resolveActionTargets } from "../../components/sidebar/sessionActionTargets";
import { EMPTY_OPTIMISTIC } from "../../lib/sidebarOptimistic";
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

  it("clears retained deletion progress without overwriting a newer row or trash lifecycle", async () => {
    const original = base({
      profile: "default",
      created_at: "2026-01-01T00:00:00Z",
      status: "Stopped",
      lifecycle_generation: 4,
      trashed_at: "2026-01-02T00:00:00Z",
      trash_lifecycle_id: "00000000-0000-4000-8000-000000000004",
    });
    const { result } = renderHook(() => useSessions());
    await act(async () => result.current.injectSession(original));
    act(() => result.current.setSessionStatus(original.id, "Deleting"));
    act(() => result.current.clearDeletingStatus([original]));
    expect(result.current.sessions).toEqual([original]);

    for (const changed of [
      { lifecycle_generation: 5 },
      { trashed_at: "2026-01-04T00:00:00Z" },
      { trash_lifecycle_id: "00000000-0000-4000-8000-000000000005" },
      { created_at: "2026-01-03T00:00:00Z" },
      { profile: "another" },
      { status: "Running" as const },
    ]) {
      const newer = { ...original, status: "Deleting" as const, ...changed };
      act(() => result.current.applySession(newer));
      act(() => result.current.clearDeletingStatus([original]));
      expect(result.current.sessions).toEqual([newer]);
    }
  });

  it("keeps the structured-view gate a mutation response does not carry", () => {
    const { result } = renderHook(() => useSessions());
    const gated = base({ view: "terminal", acp_capable: true, structured_view_off: true });
    act(() => result.current.injectSession(gated));

    // A pin response is built without the per-profile gate the list resolves.
    act(() =>
      result.current.applySession(base({ view: "terminal", acp_capable: true, pinned_at: "2026-01-01T00:00:00Z" })),
    );

    const [row] = result.current.sessions;
    expect(row?.pinned_at).toBe("2026-01-01T00:00:00Z");
    expect(resolveActionTargets([row!], EMPTY_OPTIMISTIC).switchView).toEqual([]);
  });
});

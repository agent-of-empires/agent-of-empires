// @vitest-environment jsdom

import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useSessions } from "./useSessions";
import * as api from "../lib/api";
import type { SessionResponse } from "../lib/types";

describe("useSessions / loaded sentinel", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("starts loaded=false before the first fetch resolves", () => {
    let resolveFetch: (value: api.SessionsEnvelope | null) => void = () => {};
    vi.spyOn(api, "fetchSessions").mockImplementation(() => new Promise((r) => (resolveFetch = r)));

    const { result } = renderHook(() => useSessions());

    expect(result.current.loaded).toBe(false);
    expect(result.current.sessions).toEqual([]);
    resolveFetch(null);
  });

  it.each([
    ["a successful fetch", { sessions: [], workspace_ordering: [] }, false],
    ["a failed fetch", null, true],
  ] as [string, api.SessionsEnvelope | null, boolean][])(
    "flips loaded=true after %s",
    async (_label, envelope, error) => {
      vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope);
      const { result } = renderHook(() => useSessions());
      await waitFor(() => expect(result.current.loaded).toBe(true));
      expect(result.current.error).toBe(error);
    },
  );
});

describe("useSessions / out-of-order responses", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("ignores a response from an older poll after a newer one applied", async () => {
    vi.useFakeTimers();
    const resolvers: ((value: api.SessionsEnvelope | null) => void)[] = [];
    vi.spyOn(api, "fetchSessions").mockImplementation(() => new Promise((r) => resolvers.push(r)));
    const envelope = (id: string): api.SessionsEnvelope => ({
      sessions: [{ id } as SessionResponse],
      workspace_ordering: [],
    });

    const { result } = renderHook(() => useSessions());
    await act(async () => void vi.advanceTimersByTime(3000));
    expect(resolvers).toHaveLength(2);

    await act(async () => resolvers[1]!(envelope("new")));
    await act(async () => resolvers[0]!(envelope("old")));
    expect(result.current.sessions.map((s) => s.id)).toEqual(["new"]);
  });
});

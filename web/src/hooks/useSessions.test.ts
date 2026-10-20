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
    ["a successful fetch", { sessions: [], workspace_ordering: [], cursor: { epoch: "boot", revision: 1n } }, false],
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

describe("useSessions / pushed status", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  function mountWithReads() {
    const resolvers: ((value: api.SessionsEnvelope | null) => void)[] = [];
    vi.spyOn(api, "fetchSessions").mockImplementation(() => new Promise((r) => resolvers.push(r)));
    const envelope = (status: SessionResponse["status"], revision: bigint): api.SessionsEnvelope => ({
      sessions: [{ id: "a", status } as SessionResponse],
      workspace_ordering: [],
      cursor: { epoch: "boot", revision },
    });
    return { resolvers, envelope, ...renderHook(() => useSessions()) };
  }

  it("holds a pushed status against a poll that was in flight when it landed", async () => {
    const { result, resolvers, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope("Running", 1n)));
    act(() => result.current.refresh()); // in flight when the push lands
    act(() => result.current.applyPushedStatus("a", "Waiting"));
    expect(result.current.sessions[0]!.status).toBe("Waiting");
    await act(async () => resolvers[1]!(envelope("Running", 2n)));
    expect(result.current.sessions[0]!.status).toBe("Waiting");
  });

  it("yields to a poll that began after the push", async () => {
    const { result, resolvers, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope("Running", 1n)));
    act(() => result.current.applyPushedStatus("a", "Waiting"));
    act(() => result.current.refresh());
    await act(async () => resolvers[1]!(envelope("Running", 2n)));
    expect(result.current.sessions[0]!.status).toBe("Running");
  });
});

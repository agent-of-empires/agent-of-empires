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
  afterEach(() => vi.restoreAllMocks());

  function mountWithReads() {
    const resolvers: ((value: api.SessionsEnvelope | null) => void)[] = [];
    vi.spyOn(api, "fetchSessions").mockImplementation(() => new Promise((r) => resolvers.push(r)));
    const envelope = (status: SessionResponse["status"], revision: bigint, epoch = "boot"): api.SessionsEnvelope => ({
      sessions: [{ id: "a", status } as SessionResponse],
      workspace_ordering: [],
      cursor: { epoch, revision },
    });
    return { resolvers, envelope, ...renderHook(() => useSessions()) };
  }

  const cursor = (revision: bigint, epoch = "boot") => ({ epoch, revision });

  it("applies a pushed status and drops an older read that lands after it", async () => {
    const { result, resolvers, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope("Running", 1n)));
    act(() => result.current.applyPushedStatus("a", "Waiting", cursor(2n)));
    expect(result.current.sessions[0]!.status).toBe("Waiting");
    act(() => result.current.refresh());
    await act(async () => resolvers[1]!(envelope("Running", 1n))); // started before the push, resolves after
    expect(result.current.sessions[0]!.status).toBe("Waiting");
    act(() => result.current.refresh());
    await act(async () => resolvers[2]!(envelope("Running", 3n)));
    expect(result.current.sessions[0]!.status).toBe("Running");
  });

  it("ignores a push already covered by a newer read, or from another epoch", async () => {
    const { result, resolvers, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope("Running", 5n)));
    act(() => result.current.applyPushedStatus("a", "Waiting", cursor(4n)));
    expect(result.current.sessions[0]!.status).toBe("Running");
    act(() => result.current.applyPushedStatus("a", "Waiting", cursor(9n, "other")));
    expect(result.current.sessions[0]!.status).toBe("Running");
  });

  it("applies a push from the new epoch only after a read of that epoch installed it", async () => {
    const { result, resolvers, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope("Running", 5n)));
    act(() => result.current.refresh());
    act(() => result.current.applyPushedStatus("a", "Waiting", cursor(1n, "restarted"))); // before any epoch-B read
    await act(async () => resolvers[1]!(envelope("Running", 1n, "restarted")));
    act(() => result.current.applyPushedStatus("a", "Waiting", cursor(2n, "restarted")));
    expect(result.current.sessions[0]!.status).toBe("Waiting");
  });
});

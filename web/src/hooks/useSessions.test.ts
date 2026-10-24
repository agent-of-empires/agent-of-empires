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
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  /** The next scheduled poll begins, after whatever the test did in between. */
  const nextPoll = () => act(async () => void vi.advanceTimersByTime(3000));

  function mountWithReads() {
    vi.useFakeTimers();
    const resolvers: ((value: api.SessionsEnvelope | null) => void)[] = [];
    vi.spyOn(api, "fetchSessions").mockImplementation(() => new Promise((r) => resolvers.push(r)));
    const envelope = (status: SessionResponse["status"], revision: bigint): api.SessionsEnvelope => ({
      sessions: [{ id: "a", status } as SessionResponse],
      workspace_ordering: [],
      cursor: { epoch: "boot", revision },
    });
    return { resolvers, envelope, ...renderHook(() => useSessions()) };
  }

  it("holds a pushed status against a poll the publication has not caught up with", async () => {
    const { result, resolvers, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope("Running", 1n)));
    act(() => result.current.applyPushedStatus("a", "Waiting"));
    await nextPoll(); // begins after the push, but the cached snapshot predates it
    await act(async () => resolvers[1]!(envelope("Running", 1n)));
    expect(result.current.sessions[0]!.status).toBe("Waiting");
  });

  it("releases a pushed status once a snapshot shows it, so a later differing poll applies", async () => {
    const { result, resolvers, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope("Running", 1n)));
    act(() => result.current.applyPushedStatus("a", "Waiting"));
    await nextPoll();
    await act(async () => resolvers[1]!(envelope("Waiting", 2n)));
    expect(result.current.sessions[0]!.status).toBe("Waiting");
    await nextPoll();
    await act(async () => resolvers[2]!(envelope("Running", 3n)));
    expect(result.current.sessions[0]!.status).toBe("Running");
  });

  it("drops a held status on resync, so a transition missed while disconnected cannot stick", async () => {
    const { result, resolvers, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope("Running", 1n)));
    act(() => result.current.applyPushedStatus("a", "Waiting"));
    // The stream drops, the session returns to Running unseen, and the stream reconnects.
    act(() => result.current.refresh());
    await act(async () => resolvers[1]!(envelope("Running", 1n)));
    expect(result.current.sessions[0]!.status).toBe("Running");
  });

  it("drops a held status when the runtime epoch changes", async () => {
    const { result, resolvers, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope("Running", 1n)));
    act(() => result.current.applyPushedStatus("a", "Waiting"));
    act(() => result.current.refresh());
    act(() => result.current.applyPushedStatus("a", "Waiting"));
    await act(async () => resolvers[1]!({ ...envelope("Running", 1n), cursor: { epoch: "restarted", revision: 1n } }));
    expect(result.current.sessions[0]!.status).toBe("Running");
  });
});

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
    const row = (id: string, status: SessionResponse["status"], title = id) =>
      ({ id, status, title }) as SessionResponse;
    const envelope = (rows: SessionResponse[], revision: bigint, epoch = "boot"): api.SessionsEnvelope => ({
      sessions: rows,
      workspace_ordering: [],
      cursor: { epoch, revision },
    });
    return { resolvers, row, envelope, ...renderHook(() => useSessions()) };
  }

  const cursor = (revision: bigint, epoch = "boot") => ({ epoch, revision });

  it("keeps a push over an older read that lands after it, until a newer read supersedes it", async () => {
    const { result, resolvers, row, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope([row("a", "Running")], 1n)));
    act(() => result.current.applyPushedStatus("a", "Waiting", cursor(3n)));
    expect(result.current.sessions[0]!.status).toBe("Waiting");
    act(() => result.current.refresh());
    await act(async () => resolvers[1]!(envelope([row("a", "Running")], 2n))); // began before the push
    expect(result.current.sessions[0]!.status).toBe("Waiting");
    act(() => result.current.refresh());
    await act(async () => resolvers[2]!(envelope([row("a", "Running")], 4n)));
    expect(result.current.sessions[0]!.status).toBe("Running");
  });

  it("ignores a push that a newer observation of the row already covers", async () => {
    const { result, resolvers, row, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope([row("a", "Running")], 5n)));
    act(() => result.current.applyPushedStatus("a", "Waiting", cursor(4n)));
    expect(result.current.sessions[0]!.status).toBe("Running");
  });

  it("keeps a newer pushed status when an older mutation receipt lands, and still takes its other fields", async () => {
    const { result, resolvers, row, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope([row("a", "Running")], 1n)));
    act(() => result.current.applyPushedStatus("a", "Waiting", cursor(3n)));
    act(() => result.current.applySessionMutation({ session: row("a", "Running", "renamed"), cursor: cursor(2n) }));
    expect(result.current.sessions[0]).toMatchObject({ status: "Waiting", title: "renamed" });
  });

  it("applies a push to a row whose own observation is older, whatever another row's receipt advanced", async () => {
    const { result, resolvers, row, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope([row("a", "Running"), row("b", "Running")], 1n)));
    act(() => result.current.applySessionMutation({ session: row("b", "Running", "renamed"), cursor: cursor(3n) }));
    act(() => result.current.applyPushedStatus("a", "Waiting", cursor(2n)));
    expect(result.current.sessions.find((s) => s.id === "a")!.status).toBe("Waiting");
  });

  it("holds a push from a new epoch until a read of that epoch installs it", async () => {
    const { result, resolvers, row, envelope } = mountWithReads();
    await act(async () => resolvers[0]!(envelope([row("a", "Running")], 5n)));
    act(() => result.current.refresh());
    act(() => result.current.applyPushedStatus("a", "Waiting", cursor(2n, "restarted"))); // before any epoch-B read
    expect(result.current.sessions[0]!.status).toBe("Running");
    await act(async () => resolvers[1]!(envelope([row("a", "Running")], 1n, "restarted")));
    expect(result.current.sessions[0]!.status).toBe("Waiting");
  });
});

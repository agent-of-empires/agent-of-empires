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

describe("useSessions / on-demand trash", () => {
  afterEach(() => vi.restoreAllMocks());

  const row = (id: string, trashed = false) =>
    ({ id, status: "Idle", title: id, trashed_at: trashed ? "2026-01-01T00:00:00Z" : null }) as SessionResponse;
  const envelope = (rows: SessionResponse[], revision: bigint, trashedCount = 0): api.SessionsEnvelope => ({
    sessions: rows,
    workspace_ordering: [],
    trashed_count: trashedCount,
    cursor: { epoch: "boot", revision },
  });

  it("reports the trash size without fetching its rows until a view asks", async () => {
    vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 3));
    const trashed = vi.spyOn(api, "fetchTrashedSessions");

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));

    expect(result.current.trashedCount).toBe(3);
    expect(result.current.trashLoaded).toBe(false);
    expect(trashed).not.toHaveBeenCalled();
  });

  it("shares one in-flight trash read across overlapping syncs", async () => {
    vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 1));
    const resolvers: ((value: api.SessionsEnvelope | null) => void)[] = [];
    const trashed = vi
      .spyOn(api, "fetchTrashedSessions")
      .mockImplementation(() => new Promise((resolve) => resolvers.push(resolve)));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.loadTrash());
    act(() => result.current.refresh());
    act(() => result.current.refresh());
    expect(trashed).toHaveBeenCalledTimes(1);

    await act(async () => resolvers[0]!(envelope([row("t", true)], 1n, 1)));
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a", "t"]);
  });

  it("flags a failed trash read, keeps known rows, and clears the flag when a retry lands", async () => {
    vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 1));
    const trashed = vi
      .spyOn(api, "fetchTrashedSessions")
      .mockResolvedValueOnce(null)
      .mockResolvedValue(envelope([row("t", true)], 1n, 1));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.trashError).toBe(false);

    act(() => result.current.loadTrash());
    await waitFor(() => expect(result.current.trashError).toBe(true));
    expect(result.current.trashLoaded).toBe(false);
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a"]);

    act(() => result.current.loadTrash());
    await waitFor(() => expect(result.current.trashLoaded).toBe(true));
    expect(result.current.trashError).toBe(false);
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a", "t"]);
    expect(trashed).toHaveBeenCalledTimes(2);
  });

  it("ignores a trash read of another epoch", async () => {
    vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 5n, 1));
    const trashed = vi.spyOn(api, "fetchTrashedSessions").mockResolvedValue({
      ...envelope([row("t", true)], 9n, 1),
      cursor: { epoch: "other", revision: 9n },
    });

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.loadTrash());
    await waitFor(() => expect(trashed).toHaveBeenCalledTimes(1));
    await act(async () => {});
    expect(result.current.trashLoaded).toBe(false);
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a"]);
  });

  it("lets a slow trash read land although newer live revisions arrived meanwhile", async () => {
    let revision = 1n;
    const live = vi.spyOn(api, "fetchSessions").mockImplementation(async () => envelope([row("a")], revision, 1));
    const resolvers: ((value: api.SessionsEnvelope | null) => void)[] = [];
    const trashed = vi
      .spyOn(api, "fetchTrashedSessions")
      .mockImplementation(() => new Promise((resolve) => resolvers.push(resolve)));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.loadTrash());
    // Polls keep installing newer revisions while the one trash read is still on the wire.
    for (const next of [2n, 3n, 4n]) {
      revision = next;
      act(() => result.current.refresh());
      await waitFor(() => expect(live).toHaveBeenCalledTimes(Number(next)));
    }
    expect(trashed).toHaveBeenCalledTimes(1);
    expect(result.current.trashLoaded).toBe(false);

    await act(async () => resolvers[0]!(envelope([row("t", true)], 1n, 1)));
    expect(result.current.trashLoaded).toBe(true);
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a", "t"]);
  });

  it("keeps a row's newer mutation receipt over an older trash read", async () => {
    vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 1));
    const trashed = vi.spyOn(api, "fetchTrashedSessions").mockResolvedValue(envelope([row("t", true)], 2n, 1));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.loadTrash());
    await waitFor(() => expect(result.current.trashLoaded).toBe(true));

    const renamed = { ...row("t", true), title: "renamed" };
    act(() => result.current.applySessionMutation({ session: renamed, cursor: { epoch: "boot", revision: 5n } }));

    // The next trash read was taken before that receipt and returns the old title.
    let release: (value: api.SessionsEnvelope | null) => void = () => {};
    trashed.mockImplementationOnce(() => new Promise((resolve) => (release = resolve)));
    act(() => result.current.refresh());
    await waitFor(() => expect(trashed).toHaveBeenCalledTimes(2));
    await act(async () => release(envelope([{ ...row("t", true), title: "old" }], 3n, 1)));

    expect(result.current.sessions.find((s) => s.id === "t")!.title).toBe("renamed");
  });

  it("does not roll a row back to a delayed list read older than its trash observation", async () => {
    const live = vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 0));
    vi.spyOn(api, "fetchTrashedSessions").mockResolvedValue(envelope([row("x", true)], 3n, 1));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.loadTrash());
    await waitFor(() => expect(result.current.trashLoaded).toBe(true));
    expect(result.current.sessions.find((s) => s.id === "x")!.trashed_at).not.toBeNull();

    // Restored at 2, trashed again at 3: the live read of revision 2 is delayed past the trash read of 3.
    let release: (value: api.SessionsEnvelope | null) => void = () => {};
    live.mockImplementationOnce(() => new Promise((resolve) => (release = resolve)));
    act(() => result.current.refresh());
    await waitFor(() => expect(live).toHaveBeenCalledTimes(2));
    await act(async () => release(envelope([row("a"), row("x")], 2n, 0)));

    expect(result.current.sessions.find((s) => s.id === "x")!.trashed_at).not.toBeNull();
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a", "x"]);
  });

  it("keeps a trash-observed row that a delayed list read omits", async () => {
    const live = vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 0));
    vi.spyOn(api, "fetchTrashedSessions").mockResolvedValue(envelope([row("x", true)], 3n, 1));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.loadTrash());
    await waitFor(() => expect(result.current.trashLoaded).toBe(true));

    live.mockResolvedValue(envelope([row("a")], 2n, 0));
    act(() => result.current.refresh());
    await waitFor(() => expect(live).toHaveBeenCalledTimes(2));
    await act(async () => {});
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a", "x"]);
  });

  it("keeps a held trash row that an older trash read omits after a newer receipt", async () => {
    vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 1));
    const trashed = vi.spyOn(api, "fetchTrashedSessions").mockResolvedValue(envelope([row("t", true)], 2n, 1));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.loadTrash());
    await waitFor(() => expect(result.current.trashLoaded).toBe(true));

    const renamed = { ...row("t", true), title: "renamed" };
    act(() => result.current.applySessionMutation({ session: renamed, cursor: { epoch: "boot", revision: 5n } }));

    let release: (value: api.SessionsEnvelope | null) => void = () => {};
    trashed.mockImplementationOnce(() => new Promise((resolve) => (release = resolve)));
    act(() => result.current.refresh());
    await waitFor(() => expect(trashed).toHaveBeenCalledTimes(2));
    await act(async () => release(envelope([], 3n, 0))); // taken before the receipt, so it omits the row

    expect(result.current.sessions.find((s) => s.id === "t")!.title).toBe("renamed");
  });

  it("does not bring back a row a newer trash read omitted when an older list read still carries it", async () => {
    const live = vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 1));
    const trashed = vi.spyOn(api, "fetchTrashedSessions").mockResolvedValue(envelope([row("x", true)], 3n, 1));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.loadTrash());
    await waitFor(() => expect(result.current.trashLoaded).toBe(true));
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a", "x"]);

    // x leaves the trash at 4 (purged); a list read of revision 2 that predates it is still on the wire.
    let release: (value: api.SessionsEnvelope | null) => void = () => {};
    live.mockImplementationOnce(() => new Promise((resolve) => (release = resolve)));
    trashed.mockResolvedValue(envelope([], 4n, 0));
    act(() => result.current.refresh());
    await waitFor(() => expect(live).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(trashed).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(result.current.sessions.map((s) => s.id)).toEqual(["a"]));

    await act(async () => release(envelope([row("a"), row("x")], 2n, 0)));
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a"]);

    // A list read at or past the omission is authoritative again.
    live.mockResolvedValue(envelope([row("a")], 4n, 0));
    act(() => result.current.refresh());
    await waitFor(() => expect(live).toHaveBeenCalledTimes(3));
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a"]);
  });

  it("does not reinstall a purged row from a trash read older than the live reads that showed and then omitted it", async () => {
    const live = vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 1));
    const resolvers: ((value: api.SessionsEnvelope | null) => void)[] = [];
    const trashed = vi
      .spyOn(api, "fetchTrashedSessions")
      .mockImplementation(() => new Promise((resolve) => resolvers.push(resolve)));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.loadTrash()); // trash read 1 stays on the wire

    live.mockResolvedValue(envelope([row("a"), row("x")], 2n, 0)); // x restored
    act(() => result.current.refresh());
    await waitFor(() => expect(result.current.sessions.map((s) => s.id)).toEqual(["a", "x"]));
    live.mockResolvedValue(envelope([row("a")], 4n, 0)); // x gone again
    act(() => result.current.refresh());
    await waitFor(() => expect(result.current.sessions.map((s) => s.id)).toEqual(["a"]));

    await act(async () => resolvers[0]!(envelope([row("x", true)], 1n, 1)));
    expect(result.current.trashLoaded).toBe(true);
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a"]);

    // A trash read newer than everything seen of x still lists it.
    trashed.mockResolvedValue(envelope([row("x", true)], 5n, 1));
    act(() => result.current.refresh());
    await waitFor(() => expect(result.current.sessions.map((s) => s.id)).toEqual(["a", "x"]));
  });

  it("keeps a pushed status for a trashed row through an older list read and an older trash read", async () => {
    const live = vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 1));
    const resolvers: ((value: api.SessionsEnvelope | null) => void)[] = [];
    vi.spyOn(api, "fetchTrashedSessions").mockImplementation(() => new Promise((resolve) => resolvers.push(resolve)));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.loadTrash());
    act(() => result.current.applyPushedStatus("t", "Running", { epoch: "boot", revision: 9n }));

    live.mockResolvedValue(envelope([row("a")], 2n, 1)); // an older read that lacks the trashed row
    act(() => result.current.refresh());
    await waitFor(() => expect(live).toHaveBeenCalledTimes(2));
    await act(async () => resolvers[0]!(envelope([row("t", true)], 3n, 1)));

    expect(result.current.sessions.find((s) => s.id === "t")!.status).toBe("Running");
  });

  it("reports the trash unloaded after a restart until the new epoch's trash is read", async () => {
    const live = vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 1));
    const trashed = vi.spyOn(api, "fetchTrashedSessions").mockResolvedValue(envelope([row("t", true)], 1n, 1));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.loadTrash());
    await waitFor(() => expect(result.current.trashLoaded).toBe(true));

    const restarted = (rows: SessionResponse[]): api.SessionsEnvelope => ({
      ...envelope(rows, 1n, 1),
      cursor: { epoch: "restarted", revision: 1n },
    });
    live.mockResolvedValue(restarted([row("a")]));
    trashed.mockResolvedValue(restarted([row("t", true)]));
    // Hold the new epoch's trash read so the window between the two installs is observable.
    let release: (value: api.SessionsEnvelope | null) => void = () => {};
    trashed.mockImplementationOnce(() => new Promise((resolve) => (release = resolve)));
    act(() => result.current.refresh());
    await waitFor(() => expect(result.current.runtimeEpoch).toBe("restarted"));
    expect(result.current.trashLoaded).toBe(false);

    await act(async () => release(restarted([row("t", true)])));
    await waitFor(() => expect(result.current.trashLoaded).toBe(true));
  });

  it("keeps a newer pushed status on a row the trash read brings in", async () => {
    vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 1));
    vi.spyOn(api, "fetchTrashedSessions").mockResolvedValue(envelope([row("t", true)], 2n, 1));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.applyPushedStatus("t", "Running", { epoch: "boot", revision: 3n }));
    act(() => result.current.loadTrash());
    await waitFor(() => expect(result.current.trashLoaded).toBe(true));
    expect(result.current.sessions.find((s) => s.id === "t")!.status).toBe("Running");
  });

  it("merges loaded trash rows into the list and keeps them across polls and restores", async () => {
    const live = vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope([row("a")], 1n, 1));
    const trashed = vi.spyOn(api, "fetchTrashedSessions").mockResolvedValue(envelope([row("t", true)], 1n, 1));

    const { result } = renderHook(() => useSessions());
    await waitFor(() => expect(result.current.loaded).toBe(true));
    act(() => result.current.loadTrash());
    await waitFor(() => expect(result.current.trashLoaded).toBe(true));
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a", "t"]);

    live.mockResolvedValue(envelope([row("a")], 2n, 1));
    act(() => result.current.refresh());
    await waitFor(() => expect(live).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(trashed).toHaveBeenCalledTimes(2));
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a", "t"]);

    // Restored: the live list now carries the row, and the trash no longer does.
    live.mockResolvedValue(envelope([row("a"), row("t")], 3n, 0));
    trashed.mockResolvedValue(envelope([], 3n, 0));
    act(() => result.current.refresh());
    await waitFor(() => expect(result.current.sessions.find((s) => s.id === "t")!.trashed_at).toBeNull());
    expect(result.current.sessions.map((s) => s.id)).toEqual(["a", "t"]);
  });
});

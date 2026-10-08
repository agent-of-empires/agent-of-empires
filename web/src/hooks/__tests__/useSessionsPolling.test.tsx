// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, act, cleanup } from "@testing-library/react";

vi.mock("../../lib/api", () => ({ fetchSessions: vi.fn() }));

import { useSessions } from "../useSessions";
import { fetchSessions, type SessionsEnvelope, type RuntimeCursor } from "../../lib/api";
import { isServerDown, setServerDown } from "../../lib/connectionState";

const GAP = 3000;
const DEADLINE = 15000;
const cursor = (revision: bigint, epoch = "boot"): RuntimeCursor => ({ epoch, revision });
const envelope = (ids: string[], revision = 10n, epoch = "boot"): SessionsEnvelope => ({
  sessions: ids.map((id) => ({ id, color: null })) as SessionsEnvelope["sessions"],
  workspace_ordering: ids,
  cursor: cursor(revision, epoch),
});
const ids = (result: { current: { sessions: { id: string }[] } }) => result.current.sessions.map((s) => s.id);
async function settle(ms = 0) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.mocked(fetchSessions).mockReset();
  setServerDown(false);
});
afterEach(() => {
  cleanup();
  vi.clearAllTimers();
  vi.useRealTimers();
  setServerDown(false);
});

describe("useSessions polling", () => {
  it("aborts every deadline, marks offline, and starts exactly one successor", async () => {
    const signals: AbortSignal[] = [];
    let active = 0;
    let peak = 0;
    vi.mocked(fetchSessions).mockImplementation(
      (signal) =>
        new Promise((resolve) => {
          signals.push(signal!);
          peak = Math.max(peak, ++active);
          signal!.addEventListener(
            "abort",
            () => {
              active--;
              resolve(null);
            },
            { once: true },
          );
        }),
    );
    const { result, unmount } = renderHook(() => useSessions());
    for (let i = 0; i < 3; i++) {
      await settle(DEADLINE);
      expect(signals[i]!.aborted).toBe(true);
      expect(active).toBe(0);
      expect(result.current.loaded).toBe(true);
      expect(result.current.error).toBe(true);
      expect(isServerDown()).toBe(true);
      await settle(GAP);
      expect(signals).toHaveLength(i + 2);
    }
    expect(peak).toBe(1);
    unmount();
    expect(signals.at(-1)!.aborted).toBe(true);
    await settle(DEADLINE + GAP);
    expect(signals).toHaveLength(4);
  });

  it("retains rows and ordering on timeout, recovers, and rejects the FIRST expired transport's late answer", async () => {
    const stale = Promise.withResolvers<SessionsEnvelope>();
    vi.mocked(fetchSessions)
      .mockResolvedValueOnce(envelope(["initial"]))
      .mockImplementationOnce(() => stale.promise)
      .mockResolvedValue(envelope(["fresh"], 12n));
    const { result } = renderHook(() => useSessions());
    await settle();
    await settle(GAP + DEADLINE);
    expect(ids(result)).toEqual(["initial"]);
    expect(result.current.workspaceOrdering).toEqual(["initial"]);
    expect(result.current.error).toBe(true);
    expect(vi.mocked(fetchSessions).mock.calls[1]![0]!.aborted).toBe(true);
    await settle(GAP);
    expect(ids(result)).toEqual(["fresh"]);
    expect(result.current.error).toBe(false);
    expect(isServerDown()).toBe(false);
    await act(async () => stale.resolve(envelope(["stale"], 99n)));
    expect(ids(result)).toEqual(["fresh"]);
  });

  it("aborts cleanup and StrictMode transports without publishing a connection failure", async () => {
    const signals: AbortSignal[] = [];
    vi.mocked(fetchSessions).mockImplementation(
      (signal) =>
        new Promise((resolve) => {
          signals.push(signal!);
          signal!.addEventListener("abort", () => resolve(null), { once: true });
        }),
    );
    const { result, unmount } = renderHook(() => useSessions(), {
      reactStrictMode: true,
    });
    await settle();
    expect(signals).toHaveLength(2);
    expect(signals[0]!.aborted).toBe(true);
    expect(result.current.error).toBe(false);
    unmount();
    await settle(DEADLINE + GAP);
    expect(signals[1]!.aborted).toBe(true);
    expect(signals).toHaveLength(2);
    expect(isServerDown()).toBe(false);
  });

  it("keeps one answering request in flight and schedules from its completion", async () => {
    const answer = Promise.withResolvers<SessionsEnvelope>();
    vi.mocked(fetchSessions)
      .mockImplementationOnce(() => answer.promise)
      .mockResolvedValue(envelope([]));
    renderHook(() => useSessions());
    await settle(GAP * 2);
    expect(fetchSessions).toHaveBeenCalledTimes(1);
    await act(async () => answer.resolve(envelope([])));
    await settle(GAP - 1);
    expect(fetchSessions).toHaveBeenCalledTimes(1);
    await settle(1);
    expect(fetchSessions).toHaveBeenCalledTimes(2);
  });
});

describe("connectivity independently of canonical admission", () => {
  it.each(["boot", "previous"])(
    "clears offline on a valid stale %s response without regressing state",
    async (staleEpoch) => {
      vi.mocked(fetchSessions)
        .mockResolvedValueOnce(envelope(["A", "B"], 10n, "previous"))
        .mockResolvedValue(envelope(["A", "B"]));
      const { result } = renderHook(() => useSessions());
      await settle();
      await settle(GAP);
      act(() =>
        result.current.applySessionMutation({
          session: { ...result.current.sessions[1]!, color: "green" },
          cursor: cursor(12n),
        }),
      );
      vi.mocked(fetchSessions).mockResolvedValueOnce(null);
      await settle(GAP);
      expect(result.current.error).toBe(true);
      expect(isServerDown()).toBe(true);
      vi.mocked(fetchSessions).mockResolvedValueOnce(
        envelope(["B", "A"], staleEpoch === "boot" ? 11n : 99n, staleEpoch),
      );
      await settle(GAP);
      expect(result.current.error).toBe(false);
      expect(isServerDown()).toBe(false);
      expect(ids(result)).toEqual(["A", "B"]);
      expect(result.current.workspaceOrdering).toEqual(["A", "B"]);
      expect(result.current.observedById).toEqual({ A: cursor(10n), B: cursor(12n) });
      expect(result.current.sessions[1]!.color).toBe("green");
    },
  );
});

describe("canonical session receipts", () => {
  it("installs A11 after B12, fences stale GET/order, and publishes per-id rows and observations atomically", async () => {
    vi.mocked(fetchSessions).mockResolvedValue(envelope(["A", "B"]));
    const trace: { color: string | null | undefined; revision: bigint | undefined }[] = [];
    const { result } = renderHook(() => {
      const state = useSessions();
      trace.push({
        color: state.sessions.find((row) => row.id === "A")?.color,
        revision: state.observedById.A?.revision,
      });
      return state;
    });
    await settle();
    const mutation = (id: string, revision: bigint, color: string | null) => ({
      session: { ...result.current.sessions.find((row) => row.id === id)!, color },
      cursor: cursor(revision),
    });
    act(() => result.current.applySessionMutation(mutation("B", 12n, "green")));
    expect(result.current.observedById.A).toEqual(cursor(10n));
    act(() => result.current.applySessionMutation(mutation("A", 11n, "red")));
    expect(result.current.sessions.map((row) => row.color)).toEqual(["red", "green"]);
    expect(result.current.observedById).toEqual({ A: cursor(11n), B: cursor(12n) });
    vi.mocked(fetchSessions).mockResolvedValueOnce(envelope(["B", "A"], 11n));
    await settle(GAP);
    expect(ids(result)).toEqual(["A", "B"]);
    expect(result.current.workspaceOrdering).toEqual(["A", "B"]);
    expect(result.current.sessions.map((row) => row.color)).toEqual(["red", "green"]);
    vi.mocked(fetchSessions).mockResolvedValueOnce(envelope(["B", "A"], 12n));
    await settle(GAP);
    expect(ids(result)).toEqual(["B", "A"]);
    expect(result.current.observedById).toEqual({ A: cursor(12n), B: cursor(12n) });
    expect(trace.some((row) => row.revision === 11n && row.color !== "red")).toBe(false);
  });

  it("synchronously fences same-batch GETs but compares mutation ACKs only to their own row", async () => {
    const poll = Promise.withResolvers<SessionsEnvelope>();
    vi.mocked(fetchSessions)
      .mockResolvedValueOnce(envelope(["A", "B"]))
      .mockImplementationOnce(() => poll.promise);
    const { result } = renderHook(() => useSessions());
    await settle();
    await settle(GAP);
    await act(async () => {
      result.current.applySessionMutation({
        session: { ...result.current.sessions[1]!, color: "green" },
        cursor: cursor(12n),
      });
      result.current.applySessionMutation({
        session: { ...result.current.sessions[0]!, color: "red" },
        cursor: cursor(11n),
      });
      poll.resolve(envelope(["B", "A"], 11n));
    });
    expect(ids(result)).toEqual(["A", "B"]);
    expect(result.current.sessions.map((row) => row.color)).toEqual(["red", "green"]);
    expect(result.current.observedById).toEqual({ A: cursor(11n), B: cursor(12n) });
    act(() => {
      result.current.applySessionMutation({
        session: { ...result.current.sessions[0]!, color: "amber" },
        cursor: cursor(13n),
      });
      result.current.applySessionMutation({
        session: { ...result.current.sessions[0]!, color: null },
        cursor: cursor(11n),
      });
    });
    expect(result.current.sessions[0]!.color).toBe("amber");
    expect(result.current.observedById.A).toEqual(cursor(13n));
  });

  it("restarts epochs explicitly, drops abandoned responses, and removes observations for deleted ids", async () => {
    vi.mocked(fetchSessions).mockResolvedValueOnce(envelope(["A", "B"], 100n, "old"));
    const { result } = renderHook(() => useSessions());
    await settle();
    const oldA = result.current.sessions[0]!;
    vi.mocked(fetchSessions).mockResolvedValueOnce(envelope(["A"], 1n, "new"));
    await settle(GAP);
    act(() => result.current.applySessionMutation({ session: { ...oldA, color: "red" }, cursor: cursor(999n, "old") }));
    vi.mocked(fetchSessions).mockResolvedValueOnce(envelope(["B"], 1000n, "old"));
    await settle(GAP);
    expect(ids(result)).toEqual(["A"]);
    expect(result.current.runtimeEpoch).toBe("new");
    expect(result.current.observedById).toEqual({ A: cursor(1n, "new") });
    vi.mocked(fetchSessions).mockResolvedValueOnce(envelope(["A"], 2n, "new"));
    await settle(GAP);
    expect(result.current.observedById.A).toEqual(cursor(2n, "new"));
    act(() => result.current.applySession({ ...result.current.sessions[0]!, color: "green" }));
    expect(result.current.observedById.A).toBeUndefined();
  });
});

// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";

vi.mock("../../lib/api", () => ({
  fetchSessions: vi.fn(),
}));

import { useSessions } from "../useSessions";
import { fetchSessions } from "../../lib/api";
import type { SessionsEnvelope } from "../../lib/api";

const POLL_INTERVAL = 3000;
/** Mirrors POLL_DEADLINE_MS in the hook; the file has no constant export. */
const POLL_DEADLINE = 15000;

const envelope = (ids: string[]): SessionsEnvelope => ({
  sessions: ids.map((id) => ({ id })) as SessionsEnvelope["sessions"],
  workspace_ordering: [],
});

const ids = (result: { current: { sessions: { id: string }[] } }) => result.current.sessions.map((s) => s.id);

async function settle(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

beforeEach(() => {
  vi.useFakeTimers();
  // Queued one-shot implementations would otherwise leak into the next test.
  vi.mocked(fetchSessions).mockReset();
});

afterEach(() => {
  vi.runOnlyPendingTimers();
  vi.useRealTimers();
});

describe("useSessions polling", () => {
  it("keeps polling when a request never answers", async () => {
    // The first GET /api/sessions hangs forever and is never settled.
    vi.mocked(fetchSessions)
      .mockImplementationOnce(() => new Promise<never>(() => {}))
      .mockResolvedValue(envelope(["s1"]));

    const { result } = renderHook(() => useSessions());
    await settle(0);
    expect(vi.mocked(fetchSessions)).toHaveBeenCalledTimes(1);

    // The hung request must not own the cadence: past its deadline the loop
    // asks again, and the replacement's answer reaches the list.
    await settle(POLL_DEADLINE + POLL_INTERVAL);
    expect(vi.mocked(fetchSessions).mock.calls.length).toBeGreaterThan(1);
    expect(ids(result)).toEqual(["s1"]);
  });

  it("drops a superseded request's late answer instead of rolling the list back", async () => {
    // A request that will answer only when released, well past its deadline.
    const stale = Promise.withResolvers<SessionsEnvelope>();
    vi.mocked(fetchSessions)
      .mockImplementationOnce(() => new Promise<never>(() => {}))
      .mockResolvedValueOnce(envelope(["fresh"]))
      .mockImplementationOnce(() => stale.promise);

    const { result } = renderHook(() => useSessions());
    await settle(0);

    // The replacement lands first.
    await settle(POLL_DEADLINE + POLL_INTERVAL);
    expect(ids(result)).toEqual(["fresh"]);

    // A request written off by its deadline answers much later. It is not the
    // current generation, so its older snapshot must not be applied.
    await act(async () => {
      stale.resolve(envelope(["stale"]));
    });
    expect(ids(result)).toEqual(["fresh"]);
  });

  it("keeps the nominal cadence without stacking requests", async () => {
    // A slow-but-answering daemon must still poll one request deep: the
    // replacement waits for the previous answer, it does not race it.
    let inFlight = 0;
    let peakInFlight = 0;
    const delay = Promise.withResolvers<void>();
    setTimeout(delay.resolve, POLL_INTERVAL * 2);
    vi.mocked(fetchSessions).mockImplementation(async () => {
      inFlight += 1;
      peakInFlight = Math.max(peakInFlight, inFlight);
      await delay.promise;
      inFlight -= 1;
      return envelope([]);
    });

    renderHook(() => useSessions());
    await settle(60_000);

    expect(peakInFlight).toBe(1);
    // 60s at the 3s gap, minus the request still in flight at the cut.
    expect(vi.mocked(fetchSessions).mock.calls.length).toBeGreaterThanOrEqual(19);
    expect(vi.mocked(fetchSessions).mock.calls.length).toBeLessThanOrEqual(21);
  });
});

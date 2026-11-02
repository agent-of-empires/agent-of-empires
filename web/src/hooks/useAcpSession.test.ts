// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { useLayoutEffect } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { emptyAcpState, nativeInputAvailable } from "../lib/acpTypes";
import { STATE_TTL_MS, STORAGE_KEY_PREFIX } from "../lib/acpStateStorage";
import { reportAcpInteraction, type ServerQueuedPrompt } from "../lib/api";
import { useCancelEscalation } from "../components/acp/useCancelEscalation";
import { ACP_DIAL_TIMEOUT_MS, ACP_MAX_RETRIES, ACP_WS_STALE_MS, acpRetryDelayMs } from "./acpSession/useAcpConnection";
import {
  FakeWebSocket,
  currentControlSnapshot,
  flushAsync,
  installAcpFakes,
  json,
  lastSocket,
  profileWrapper,
  sockets,
  type Call,
  type Route,
} from "./__tests__/acpHarness";
import { clearAcpCache, useAcpSession } from "./useAcpSession";

vi.mock("../lib/api", async (importActual) => ({
  ...(await importActual<typeof import("../lib/api")>()),
  reportAcpInteraction: vi.fn(),
}));

type HookArgs = Parameters<typeof useAcpSession>;

let calls: Call[];

beforeEach(() => {
  localStorage.clear();
  clearAcpCache();
  calls = installAcpFakes();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  clearAcpCache();
});

function render(...args: HookArgs) {
  return renderHook(() => useAcpSession(...args), { wrapper: profileWrapper });
}

async function openSessionWithReceipt(receipt: Parameters<typeof currentControlSnapshot>[1], ...args: HookArgs) {
  const hook = render(...args);
  await flushAsync();
  lastSocket().open();
  await flushAsync();
  lastSocket().message(currentControlSnapshot(hook.result.current.state, receipt));
  return hook;
}

async function openSession(...args: HookArgs) {
  return openSessionWithReceipt({ ready: true }, ...args);
}

const posts = (fragment: string) => calls.filter((c) => c.method === "POST" && c.url.includes(fragment));

describe("reconnect (#1130)", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    FakeWebSocket.closeFiresOnClose = true;
  });

  it.each([
    [1, 1000],
    [2, 2000],
    [5, 16000],
    [6, 30000],
    [0, 1000],
  ])("acpRetryDelayMs(%i) = %i", (attempt, ms) => {
    expect(acpRetryDelayMs(attempt)).toBe(ms);
  });

  it("backs off after a close, then resets once a socket opens", async () => {
    const { result } = render("sess-1");
    await flushAsync();
    sockets[0]!.drop();
    expect(result.current).toMatchObject({ reconnecting: true, retryCount: 1 });
    expect(result.current.retryCountdown).toBeGreaterThanOrEqual(1);

    await act(() => vi.advanceTimersByTimeAsync(acpRetryDelayMs(1)));
    await flushAsync();
    expect(sockets).toHaveLength(2);
    sockets[1]!.open();
    expect(result.current).toMatchObject({ reconnecting: false, retryCount: 0 });
  });

  it("stops after the retry limit and dials again on manualReconnect", async () => {
    const { result } = render("sess-2");
    await flushAsync();
    for (let attempt = 1; attempt <= ACP_MAX_RETRIES; attempt++) {
      lastSocket().drop();
      if (attempt < ACP_MAX_RETRIES) {
        await act(() => vi.advanceTimersByTimeAsync(acpRetryDelayMs(attempt)));
        await flushAsync();
      }
    }
    lastSocket().drop();
    expect(result.current).toMatchObject({ reconnecting: false, retryCount: ACP_MAX_RETRIES });

    const before = sockets.length;
    act(() => result.current.manualReconnect());
    await flushAsync();
    expect(sockets).toHaveLength(before + 1);
    expect(result.current.retryCount).toBe(0);
  });

  it("dials anyway when the replay fetch hangs", async () => {
    calls = installAcpFakes((call) => (call.url.includes("/acp/replay") ? new Promise<Response>(() => {}) : undefined));
    FakeWebSocket.closeFiresOnClose = true;
    render("sess-hung-replay");
    await flushAsync();
    expect(sockets).toHaveLength(0);
    await act(() => vi.advanceTimersByTimeAsync(ACP_DIAL_TIMEOUT_MS));
    await flushAsync();
    expect(sockets).toHaveLength(1);
  });

  it("drops a replay response that lands after the dial timeout and a newer socket row", async () => {
    const row = { id: "row-1", kind: "user_prompt", at: "t" };
    const pending: Array<() => void> = [];
    calls = installAcpFakes(({ url }) => {
      if (!url.includes("/acp/replay")) return undefined;
      const rows = url.includes("view=rows") ? [{ ...row, text: "stale" }] : [];
      return new Promise<Response>((resolve) => {
        pending.push(() => resolve(json({ frames: [], rows, lost: false, highest_seq: 1, next_cursor: 1 })));
      });
    });
    const { result } = render("sess-late-replay");
    await flushAsync();
    await act(() => vi.advanceTimersByTimeAsync(ACP_DIAL_TIMEOUT_MS));
    await flushAsync();
    expect(sockets).toHaveLength(1);
    sockets[0]!.open();
    sockets[0]!.message({ kind: "transcript_delta", delta: { Append: { ...row, text: "fresh" } } });
    expect(result.current.state.activity).toMatchObject([{ id: "row-1", text: "fresh" }]);

    pending.forEach((release) => release());
    await flushAsync();
    expect(result.current.state.activity).toMatchObject([{ id: "row-1", text: "fresh" }]);
  });

  it("abandons a socket stuck connecting and backs off", async () => {
    const { result } = render("sess-hung-connect");
    await flushAsync();
    await act(() => vi.advanceTimersByTimeAsync(ACP_DIAL_TIMEOUT_MS));
    expect(sockets[0]!.readyState).toBe(FakeWebSocket.CLOSED);
    expect(result.current).toMatchObject({ reconnecting: true, retryCount: 1 });
    await act(() => vi.advanceTimersByTimeAsync(acpRetryDelayMs(1)));
    await flushAsync();
    expect(sockets).toHaveLength(2);
  });

  it("does not show a stale retry count while redialing after resume", async () => {
    const { result } = render("sess-resume");
    await flushAsync();
    sockets[0]!.drop();
    expect(result.current.reconnecting).toBe(true);
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(result.current).toMatchObject({ reconnecting: false, retryCount: 0, status: "connecting" });
  });

  const replayedPrompt = (sid: string, seq: number) => ({
    frame: { session_id: sid, seq, event: { UserPromptSent: { text: `p${seq}` } } },
    row: { id: `user-seq-${seq}`, kind: "user_prompt", at: "t", text: `p${seq}` },
  });

  it("flags history arriving over replay while the socket is down, until a socket opens", async () => {
    const { frame, row } = replayedPrompt("sess-replay-down", 7);
    calls = installAcpFakes(({ url }) =>
      url.includes("/acp/replay")
        ? json({ frames: [frame], rows: [row], lost: false, highest_seq: 7, next_cursor: 7 })
        : undefined,
    );
    FakeWebSocket.closeFiresOnClose = true;
    const { result } = render("sess-replay-down");
    await flushAsync();
    sockets[0]!.drop();
    expect(result.current).toMatchObject({ reconnecting: true, replayedWhileDown: true });

    await act(() => vi.advanceTimersByTimeAsync(acpRetryDelayMs(1)));
    await flushAsync();
    sockets[1]!.open();
    expect(result.current.replayedWhileDown).toBe(false);
  });

  // A page that landed stays on screen whatever happens to the later pages.
  it.each<[string, () => Response | Promise<Response>]>([
    ["completes", () => json({ frames: [], rows: [], lost: false, highest_seq: 30, next_cursor: 30 })],
    ["fails", () => new Response("boom", { status: 500 })],
    ["hangs until the dial timeout", () => new Promise<Response>(() => {})],
  ])("flags history replayed forward onto loaded history when the second page %s", async (label, secondPage) => {
    const sid = "sess-replay-loaded";
    const first = replayedPrompt(sid, 6);
    const forward = replayedPrompt(sid, 11);
    let secondPageRequests = 0;
    calls = installAcpFakes(({ url }) => {
      if (!url.includes("/acp/replay")) return undefined;
      const since = new URL(url, "http://x").searchParams.get("since");
      if (since === null) {
        return json({ frames: [first.frame], rows: [first.row], lost: false, highest_seq: 10, next_cursor: 6 });
      }
      if (Number(since) > 10) {
        secondPageRequests += 1;
        return secondPage();
      }
      return json({
        frames: [forward.frame],
        rows: [forward.row],
        lost: false,
        highest_seq: 30,
        next_cursor: 11,
        has_more: true,
      });
    });
    FakeWebSocket.closeFiresOnClose = true;
    const { result } = render(sid);
    await flushAsync();
    sockets[0]!.open();
    expect(result.current).toMatchObject({ replayedWhileDown: false, state: { oldestSeq: 6 } });

    sockets[0]!.drop();
    await act(() => vi.advanceTimersByTimeAsync(acpRetryDelayMs(1)));
    // The first page commits only once the replay has gone on to ask for the second.
    await act(() => vi.waitFor(() => expect(secondPageRequests).toBeGreaterThan(0)));
    await flushAsync();
    expect(result.current.state.activity.map((r) => r.id)).toContain(forward.row.id);
    if (label.startsWith("hangs")) {
      expect(sockets).toHaveLength(1);
      expect(result.current.replayedWhileDown).toBe(false);
      await act(() => vi.advanceTimersByTimeAsync(ACP_DIAL_TIMEOUT_MS));
      await flushAsync();
    }
    expect(sockets).toHaveLength(2);
    expect(result.current).toMatchObject({ reconnecting: true, replayedWhileDown: true });
  });

  it("does not flag a reconnect whose replay only repeats the overlap window", async () => {
    const sid = "sess-replay-overlap";
    const known = replayedPrompt(sid, 6);
    calls = installAcpFakes(({ url }) => {
      if (!url.includes("/acp/replay")) return undefined;
      const since = new URL(url, "http://x").searchParams.get("since");
      // The forward request re-reads an overlap window, so the page repeats a frame already held.
      const next = since === null ? { next_cursor: 6 } : { next_cursor: 10 };
      return json({ frames: [known.frame], rows: [known.row], lost: false, highest_seq: 6, ...next });
    });
    FakeWebSocket.closeFiresOnClose = true;
    const { result } = render(sid);
    await flushAsync();
    sockets[0]!.open();
    sockets[0]!.drop();
    await act(() => vi.advanceTimersByTimeAsync(acpRetryDelayMs(1)));
    await flushAsync();
    expect(sockets).toHaveLength(2);
    expect(calls.some((c) => new URL(c.url, "http://x").searchParams.has("since"))).toBe(true);
    expect(result.current).toMatchObject({ reconnecting: true, replayedWhileDown: false });
  });

  it("does not flag a replay that brought nothing new", async () => {
    const { result } = render("sess-replay-empty");
    await flushAsync();
    sockets[0]!.drop();
    expect(result.current).toMatchObject({ reconnecting: true, replayedWhileDown: false });
  });

  it("dials without an elevation preflight", async () => {
    render("sess-no-preflight");
    await flushAsync();
    expect(sockets).toHaveLength(1);
    expect(calls.some((c) => c.url.includes("/api/login/status"))).toBe(false);
  });
});

describe("liveness watchdog (#2287)", () => {
  beforeEach(() => vi.useFakeTimers());

  it("redials a stale OPEN socket", async () => {
    await openSession("sess-zombie");
    await act(() => vi.advanceTimersByTimeAsync(ACP_WS_STALE_MS + 15000));
    await flushAsync();
    expect(sockets[0]!.readyState).toBe(FakeWebSocket.CLOSED);
    expect(sockets.length).toBeGreaterThanOrEqual(2);
  });

  it("leaves a socket alone while heartbeats arrive", async () => {
    await openSession("sess-alive");
    for (let i = 0; i < 4; i++) {
      await act(() => vi.advanceTimersByTimeAsync(30000));
      sockets[0]!.message({ kind: "heartbeat" });
    }
    await flushAsync();
    expect(sockets).toHaveLength(1);
    expect(sockets[0]!.readyState).toBe(FakeWebSocket.OPEN);
  });

  it("never redials a socket that is still connecting within the dial deadline", async () => {
    render("sess-connecting");
    await flushAsync();
    await act(() => vi.advanceTimersByTimeAsync(ACP_DIAL_TIMEOUT_MS - 1000));
    await flushAsync();
    expect(sockets).toHaveLength(1);
  });
});

describe("persisted state hydration", () => {
  it.each([
    ["resumes from a persisted lastSeq", 0, 4242],
    ["ignores an entry older than the TTL", STATE_TTL_MS + 1000, 0],
  ])("%s", async (_label, age, since) => {
    const state = { ...emptyAcpState(), lastSeq: 4242 };
    localStorage.setItem(`${STORAGE_KEY_PREFIX}sess`, JSON.stringify({ savedAt: Date.now() - age, state }));
    const replay = json({ frames: [], lost: false, highest_seq: since });
    installAcpFakes(({ url }) => (url.includes("/acp/replay") ? replay.clone() : undefined));
    render("sess");
    await flushAsync();
    expect(sockets).toHaveLength(1);
    expect(sockets[0]!.url).toContain(`/acp/ws?since=${since}`);
  });
});

describe("mid-turn cache recovery (#4530)", () => {
  it("dials from the cursor the recovery caught up to, not the cached one", async () => {
    const activity = [{ id: "t", kind: "tool_start", text: "t", at: "t" }];
    const state = { ...emptyAcpState(), lastSeq: 900, oldestSeq: 40, activity };
    localStorage.setItem(`${STORAGE_KEY_PREFIX}sess-mid`, JSON.stringify({ savedAt: Date.now(), state }));
    const rows = [
      { id: "u", kind: "user_prompt", at: "t", text: "u" },
      { id: "t2", kind: "tool_start", at: "t", text: "t2" },
    ];
    installAcpFakes(({ url }) => {
      if (!url.includes("/acp/replay")) return undefined;
      const params = new URL(url, "http://x").searchParams;
      const page = params.has("before")
        ? { next_cursor: 2100, has_more: false, rows: params.get("view") === "rows" ? rows : null }
        : { rows: params.get("view") === "rows" ? [] : null };
      return json({ frames: [], lost: false, highest_seq: 3000, ...page });
    });
    render("sess-mid");
    await flushAsync();
    expect(sockets).toHaveLength(1);
    expect(sockets[0]!.url).toContain("/acp/ws?since=3000");
  });
});

describe("recent-first cold open + loadOlder (#2236)", () => {
  const prompt = (seq: number) => ({ session_id: "sess-rf", seq, event: { UserPromptSent: { text: `p${seq}` } } });
  const page = (seqs: number[], next: number | null, hasMore: boolean) => {
    const frames = seqs.map(prompt);
    const rows = seqs.map((seq) => ({ id: `user-seq-${seq}`, kind: "user_prompt", at: "t", text: `p${seq}` }));
    return json({ frames, rows, lost: false, highest_seq: 10, next_cursor: next, has_more: hasMore });
  };
  const caps = {
    session_id: "sess-rf",
    seq: 1,
    event: { PromptCapabilities: { image: true, audio: false, embedded_context: true } },
  };
  const replay: Route = ({ url }) => {
    if (!url.includes("/acp/replay")) return undefined;
    const before = Number(new URL(url, "http://x").searchParams.get("before") ?? NaN);
    if (before <= 10) return page([2, 3, 4, 5], 2, false);
    if (before > 10) return page([6, 7, 8, 9, 10], 6, true);
    return json({ frames: [caps], rows: [], lost: false, highest_seq: 10, next_cursor: 1, has_more: true });
  };
  const ids = (seqs: number[]) => seqs.map((s) => `user-seq-${s}`);

  it("renders the tail, backfills the handshake, then prepends older history", async () => {
    installAcpFakes(replay);
    const { result } = render("sess-rf");
    await flushAsync();
    expect(result.current.state.activity.map((r) => r.id)).toEqual(ids([6, 7, 8, 9, 10]));
    expect(result.current.hasMoreOlder).toBe(true);
    expect(result.current.state.promptCapabilities).toEqual({
      image: true,
      audio: false,
      embeddedContext: true,
      steering: false,
    });
    expect(result.current.state.oldestSeq).toBe(6);

    await act(() => result.current.loadOlder());
    expect(result.current.state.activity.map((r) => r.id)).toEqual(ids([2, 3, 4, 5, 6, 7, 8, 9, 10]));
    expect(result.current.state.oldestSeq).toBe(2);
    expect(result.current.hasMoreOlder).toBe(false);
  });

  it("flags a lagged transcript when the tail reports lost", async () => {
    installAcpFakes(({ url }) =>
      url.includes("/acp/replay") ? json({ frames: [], lost: true, highest_seq: 9999, has_more: false }) : undefined,
    );
    const { result } = render("sess-lost");
    await flushAsync();
    expect(result.current.state.lagged).toBe(true);
    expect(result.current.hasMoreOlder).toBe(false);
  });

  it("resets older-paging state when the session changes", async () => {
    installAcpFakes(replay);
    const { result, rerender } = renderHook(({ id }: { id: string }) => useAcpSession(id), {
      initialProps: { id: "sess-rf" },
    });
    await flushAsync();
    expect(result.current.hasMoreOlder).toBe(true);
    rerender({ id: "sess-rf-2" });
    expect(result.current.loadingOlder).toBe(false);
    await flushAsync(20);
    expect(result.current.hasMoreOlder).toBe(true);
  });
});

describe("sendPrompt outcomes", () => {
  it.each([
    ["a 500", () => new Response("boom", { status: 500 })],
    [
      "a network exception",
      () => {
        throw new TypeError("Failed to fetch");
      },
    ],
  ])("%s does not leave the spinner latched (#3417)", async (_label, respond) => {
    calls = installAcpFakes(({ url }) => (url.includes("/acp/prompt") ? respond() : undefined));
    const { result, unmount } = await openSession("s-1");
    await act(() => result.current.sendPrompt("hello"));
    expect(posts("/acp/prompt")).toHaveLength(1);
    expect(result.current.state.turnActive).toBe(false);
    unmount();
    expect(sockets.every((s) => s.readyState === FakeWebSocket.CLOSED)).toBe(true);
  });

  describe("auto-wake (#1581)", () => {
    const failingWake =
      (suffix: string): Route =>
      ({ url, method }) => {
        if (url.endsWith(suffix) && method === "PATCH") return new Response("simulated failure", { status: 500 });
        if (url.includes("/acp/prompt")) return json({ disposition: "queued", queued_id: "srv-queued-1" }, 202);
        return undefined;
      };
    const queuedPrompt: Route = ({ url }) =>
      url.includes("/acp/prompt") ? json({ disposition: "queued", queued_id: "srv-queued-1" }, 202) : undefined;
    const patches = (suffix: string) => calls.filter((c) => c.method === "PATCH" && c.url.endsWith(suffix));

    it("wakes a snoozed session before sending", async () => {
      calls = installAcpFakes(queuedPrompt);
      const { result } = await openSessionWithReceipt(
        { ready: false, wakeEligible: true },
        "sess-wake",
        "absent",
        null,
        "2099-01-01T00:00:00Z",
      );
      await flushAsync();
      await act(() => result.current.sendPrompt("wake me up"));
      await flushAsync();
      expect(patches("/snooze").map((c) => JSON.parse(c.body!))).toEqual([{ minutes: null }]);
      expect(result.current.state.queuedPrompts.map((q) => q.text)).toEqual(["wake me up"]);
    });

    // #4116: sending never unarchives; the user must unarchive first.
    it.each([
      ["archived", null],
      ["archived and snoozed", "2099-01-01T00:00:00Z"],
    ])("refuses to send to an %s session", async (_label, snoozed) => {
      calls = installAcpFakes(queuedPrompt);
      const { result } = render("sess-archived", "absent", "2026-01-01T00:00:00Z", snoozed);
      await flushAsync();
      await act(() => result.current.sendPrompt("wake me up"));
      await flushAsync();
      expect(calls.filter((c) => c.method === "PATCH")).toHaveLength(0);
      expect(posts("/acp/prompt")).toHaveLength(0);
      expect(result.current.state.interactionError?.message).toBe("session is archived; unarchive it first");
    });

    it("does not call wake endpoints for a live session", async () => {
      calls = installAcpFakes(queuedPrompt);
      const { result } = await openSession("sess-live", "absent", null, null);
      await flushAsync();
      await act(() => result.current.sendPrompt("just a prompt"));
      await flushAsync();
      expect(calls.filter((c) => c.method === "PATCH")).toHaveLength(0);
      expect(result.current.state.queuedPrompts).toHaveLength(1);
    });

    it("sends nothing when the snooze wake fails", async () => {
      calls = installAcpFakes(failingWake("/snooze"));
      const { result } = await openSessionWithReceipt(
        { ready: false, wakeEligible: true },
        "sess-wake-fail",
        "absent",
        null,
        "2099-01-01T00:00:00Z",
      );
      await flushAsync();
      await act(() => result.current.sendPrompt("wake me up"));
      await flushAsync();
      expect(posts("/acp/prompt")).toHaveLength(0);
      expect(result.current.state.queuedPrompts).toHaveLength(0);
      expect(result.current.state.interactionError?.message).toMatch(/wake/i);
    });
  });

  // A 503 that is not `worker_not_ready` is a real failure: it keeps its banner and the prompt
  // is not silently parked on the queue.
  it("does not re-enqueue a 503 that is not worker_not_ready", async () => {
    calls = installAcpFakes(({ url, method }) => {
      if (url.includes("/acp/replay")) return json({ frames: [], lost: false, highest_seq: 0 });
      if (url.includes("/acp/prompt")) return new Response("worker_capacity_full (8/8)", { status: 503 });
      if (url.includes("/queue") && method === "GET") return json([]);
      return undefined;
    });
    const { result } = await openSession("sess-capacity", "running", null, null);
    await flushAsync();
    await act(() => result.current.sendPrompt("no room at the inn"));
    await flushAsync();
    expect(posts("/queue")).toHaveLength(0);
    expect(result.current.state.interactionError?.message).toMatch(/capacity/i);
  });

  // Both park kinds are sendable at `dispatch::decide`, and a still-limited spawn answers 503
  // `worker_not_ready`, which re-queues the prompt. The armed park is the one a weekly limit
  // produces (#3688 covers only the redelivery cap).
  describe.each([
    ["rate_limit_exhausted_retries", "rateLimitRetriesExhausted", "cap"],
    ["rate_limited", "rateLimitParked", "armed"],
  ] as const)("rate-limit park: %s", (reason, flag, tag) => {
    const parked =
      (isParked: boolean): Route =>
      ({ url, method }) => {
        if (url.includes("/acp/replay")) {
          const frames = isParked ? [{ session_id: `sess-${tag}`, seq: 1, event: { Stopped: { reason } } }] : [];
          return json({ frames, lost: false, highest_seq: frames.length });
        }
        if (url.includes("/acp/prompt")) return new Response("worker_not_ready", { status: 503 });
        if (url.includes("/queue") && method === "GET") return json([]);
        return undefined;
      };

    // Replay flags are not admission. The explicit current server governor can
    // report a park the client has not learned about yet.
    it.each([[true], [false]])("parked=%s re-enqueues a 503'd prompt", async (isParked) => {
      calls = installAcpFakes(parked(isParked));
      const { result } = await openSessionWithReceipt(
        { ready: false, wakeEligible: true },
        `sess-${tag}`,
        "running",
        null,
        null,
      );
      await flushAsync();
      expect(result.current.state[flag]).toBe(isParked);
      await act(() => result.current.sendPrompt("try again after the park"));
      await flushAsync();
      expect(posts("/queue")).toHaveLength(1);
      expect(result.current.state.queuedPrompts.map((q) => q.text)).toEqual(["try again after the park"]);
    });

    it("offers Send now on a row stranded behind the park", async () => {
      installAcpFakes(parked(true));
      const { result } = await openSessionWithReceipt(
        { ready: false, wakeEligible: true },
        `sess-${tag}-send`,
        "absent",
        null,
        null,
      );
      expect(result.current.status).toBe("open");
      expect(result.current.canSendQueuedNow).toBe(true);
    });
  });
});

describe("server queue", () => {
  let serverQueue: Map<string, ServerQueuedPrompt>;
  let serverBusy: boolean;
  let workerDown: boolean;
  const row = (id: string, text: string, extra: Partial<ServerQueuedPrompt> = {}): ServerQueuedPrompt => ({
    id,
    seq: serverQueue.size,
    text,
    created_at: "2026-01-01T00:00:00.000Z",
    ...extra,
  });

  beforeEach(() => {
    serverQueue = new Map();
    serverBusy = false;
    workerDown = false;
    vi.mocked(reportAcpInteraction).mockClear();
    calls = installAcpFakes(({ url, method, body }) => {
      if (url.includes("/queue")) {
        const item = url.match(/\/queue\/([^/?]+)/)?.[1];
        if (method === "GET") return json([...serverQueue.values()].sort((a, b) => a.seq - b.seq));
        if (method === "POST") {
          const parsed = JSON.parse(body!) as { id: string; text: string };
          serverQueue.set(parsed.id, row(parsed.id, parsed.text));
          return json(serverQueue.get(parsed.id));
        }
        if (method === "DELETE" && item && !serverQueue.delete(decodeURIComponent(item))) {
          return new Response("queued prompt not found", { status: 404 });
        }
        if (method === "DELETE" && !item) serverQueue.clear();
        return new Response(null, { status: 204 });
      }
      if (url.includes("/acp/prompt") && workerDown) return new Response("worker_not_ready", { status: 503 });
      if (url.includes("/acp/prompt") && body && JSON.parse(body).queued_prompt_id) {
        // The daemon consumes the named row, or reports it already delivered.
        const consumed = serverQueue.delete(JSON.parse(body).queued_prompt_id);
        return consumed ? json({ disposition: "sent" }, 202) : new Response("queued_prompt_gone", { status: 409 });
      }
      if (url.includes("/acp/prompt") && serverBusy) {
        const parsed = JSON.parse(body!) as { prompt_id: string; text: string };
        serverQueue.set(parsed.prompt_id, row(parsed.prompt_id, parsed.text));
        return json({ disposition: "queued", queued_id: parsed.prompt_id }, 202);
      }
      if (url.includes("/acp/prompt")) return json({ disposition: "sent" }, 202);
      return undefined;
    });
  });

  async function openBusy(id: string) {
    const hook = await openSession(id);
    serverBusy = true;
    lastSocket().message({ session_id: id, seq: 1, event: { UserPromptSent: { text: "kick" } } });
    await flushAsync();
    expect(hook.result.current.state.turnActive).toBe(true);
    act(() => void hook.result.current.sendPrompt("follow-up"));
    await flushAsync();
    return hook;
  }

  it("shows the row the daemon reports when it parks a busy-turn prompt", async () => {
    const { result } = await openBusy("sess-busy");
    expect(posts("/acp/prompt").map((c) => JSON.parse(c.body!).text)).toEqual(["follow-up"]);
    expect(posts("/queue")).toHaveLength(0);
    expect(result.current.state.queuedPrompts).toMatchObject([{ text: "follow-up", pending: false }]);
    expect(reportAcpInteraction).toHaveBeenCalledWith("prompt_queued");
  });

  it("POSTs directly when the session is idle", async () => {
    const { result } = await openSession("sess-idle");
    await act(() => result.current.sendPrompt("send now"));
    expect(posts("/acp/prompt")).toHaveLength(1);
    expect(posts("/queue")).toHaveLength(0);
  });

  it("mirrors edit, remove, and clear to the server", async () => {
    const { result } = await openBusy("sess-mut");
    const id = result.current.state.queuedPrompts[0]!.id;
    act(() => result.current.editQueuedPrompt(id, "row edited"));
    act(() => result.current.removeQueuedPrompt(id));
    await flushAsync();
    expect(calls.filter((c) => c.url.includes(`/queue/${encodeURIComponent(id)}`)).map((c) => c.method)).toEqual([
      "PATCH",
      "DELETE",
    ]);
    expect(result.current.state.queuedPrompts).toEqual([]);
    act(() => result.current.clearQueue());
    await flushAsync();
    expect(calls.some((c) => c.method === "DELETE" && /\/queue$/.test(c.url.split("?")[0]!))).toBe(true);
  });

  it("hydrates from the server snapshot on connect", async () => {
    serverQueue.set("pre", row("pre", "migrated"));
    const { result } = await openSession("sess-migrate");
    expect(calls.some((c) => c.method === "GET" && c.url.includes("/queue"))).toBe(true);
    expect(result.current.state.queuedPrompts.map((q) => q.text)).toEqual(["migrated"]);
  });

  it("re-POSTs only unconfirmed cached rows; a confirmed row the server drained stays gone", async () => {
    const queuedAt = "2026-01-01T00:00:00.000Z";
    const state = {
      ...emptyAcpState(),
      queuedPrompts: [
        { id: "drained", text: "already delivered", queuedAt, pending: false },
        { id: "unsent", text: "never reached server", queuedAt, pending: true },
      ],
    };
    localStorage.setItem(`${STORAGE_KEY_PREFIX}sess-cached`, JSON.stringify({ savedAt: Date.now(), state }));
    const { result } = await openSession("sess-cached");
    expect(posts("/queue").map((c) => JSON.parse(c.body!).id)).toEqual(["unsent"]);
    expect(serverQueue.has("drained")).toBe(false);
    expect(result.current.state.queuedPrompts.map((q) => q.id)).toEqual(["unsent"]);
  });

  it.each([
    ["legacy", false],
    ["legacy", true],
    ["lost", false],
    ["lost", true],
  ] as const)(
    "holds cached intent across %s refusal, then migrates on governed=%s eligibility",
    async (refusal, governed) => {
      const id = `sess-cache-${refusal}-${governed}`;
      const queuedAt = "2026-01-01T00:00:00.000Z";
      localStorage.setItem(
        `${STORAGE_KEY_PREFIX}${id}`,
        JSON.stringify({
          savedAt: Date.now(),
          state: {
            ...emptyAcpState(),
            connectionReady: true,
            nativeSnapshotSeq: 42,
            queuedPrompts: [
              { id: "unsent", text: "cached intent", queuedAt, pending: true },
              { id: "drained", text: "already delivered", queuedAt, pending: false },
            ],
          },
        }),
      );
      const { result } = render(id);
      await flushAsync();
      lastSocket().open();
      await flushAsync();
      expect(calls.some((call) => call.method === "GET" && call.url.includes("/queue"))).toBe(true);
      expect(posts("/queue")).toHaveLength(0);
      expect(result.current.state.queuedPrompts).toMatchObject([{ id: "unsent", pending: true }]);

      lastSocket().message(
        currentControlSnapshot(result.current.state, {
          ready: false,
          unavailable: refusal === "lost" ? { source: "native", cause: "stdout_eof", phase: "established" } : null,
        }),
      );
      await flushAsync();
      expect(posts("/queue")).toHaveLength(0);
      expect(result.current.state.queuedPrompts).toMatchObject([{ id: "unsent", pending: true }]);

      lastSocket().message(
        currentControlSnapshot(result.current.state, {
          ready: !governed,
          wakeEligible: governed,
        }),
      );
      await flushAsync();
      expect(posts("/queue").map((call) => JSON.parse(call.body!).id)).toEqual(["unsent"]);
      expect(result.current.state.queuedPrompts).toMatchObject([{ id: "unsent" }]);
      expect(result.current.state.queuedPrompts[0]?.pending).not.toBe(true);
      expect(serverQueue.has("drained")).toBe(false);
      expect(posts("/acp/prompt")).toHaveLength(0);

      lastSocket().message(currentControlSnapshot(result.current.state, { ready: !governed, wakeEligible: governed }));
      await flushAsync();
      expect(posts("/queue")).toHaveLength(1);
    },
  );

  describe("sendQueuedNow", () => {
    async function sendNow(id: string, prepare: () => void = () => {}) {
      const { result } = await openSession(`sess-${id}`);
      const queued = result.current.state.queuedPrompts.find((q) => q.id === id)!;
      prepare();
      calls.length = 0;
      await act(() => result.current.sendQueuedNow(queued));
      await flushAsync();
      return result;
    }
    const deletes = () => calls.filter((c) => c.method === "DELETE");

    it("names the row in the send so the daemon consumes it and keeps its sender", async () => {
      serverQueue.set("t1", row("t1", "text only"));
      await sendNow("t1");
      expect(deletes()).toHaveLength(0);
      const sent = posts("/acp/prompt");
      expect(sent).toHaveLength(1);
      expect(JSON.parse(sent[0]!.body!)).toMatchObject({ text: "text only", queued_prompt_id: "t1" });
      expect(serverQueue.has("t1")).toBe(false);
    });

    it("leaves a row whose attachment bytes live only on the server", async () => {
      const attachments = [{ id: "a", kind: "image" as const, mime_type: "image/png", name: "s.png", size: 9 }];
      serverQueue.set("img", row("img", "caption", { attachments }));
      const result = await sendNow("img");
      expect([deletes(), posts("/acp/prompt")]).toEqual([[], []]);
      expect(result.current.state.queuedPrompts.map((q) => q.id)).toContain("img");
    });

    it("keeps the one server row when the send is refused, instead of queueing a copy", async () => {
      serverQueue.set("w1", row("w1", "waiting"));
      const result = await sendNow("w1", () => (workerDown = true));
      expect(posts("/queue")).toHaveLength(0);
      expect(serverQueue.has("w1")).toBe(true);
      expect(result.current.state.queuedPrompts.map((q) => q.id)).toEqual(["w1"]);
    });

    it("drops the row quietly when the drain claimed it first", async () => {
      serverQueue.set("r1", row("r1", "raced"));
      const result = await sendNow("r1", () => serverQueue.delete("r1"));
      expect(posts("/queue")).toHaveLength(0);
      expect(result.current.state.queuedPrompts.map((q) => q.id)).not.toContain("r1");
      expect(result.current.state.lastError).toBeNull();
      expect(result.current.state.interactionError).toBeNull();
    });
  });
});

describe("setConfigOption", () => {
  it("posts the config id and value", async () => {
    const { result } = await openSession("sess-cfg");
    await flushAsync();
    await act(() => result.current.setConfigOption("model", "claude-sonnet-4-6"));
    const [post] = posts("/api/sessions/sess-cfg/acp/config-option");
    expect(JSON.parse(post!.body!)).toEqual({ config_id: "model", value: "claude-sonnet-4-6" });
  });

  it.each([
    ["a non-OK response", () => new Response("simulated failure", { status: 500 }), /Could not set model/],
    [
      "a network failure",
      () => {
        throw new TypeError("network down");
      },
      /Network error setting model/,
    ],
  ])("clears pending and reports %s", async (_label, respond, error) => {
    installAcpFakes(({ url }) => (url.includes("/acp/config-option") ? respond() : undefined));
    const { result } = await openSession("sess-cfg-fail");
    await flushAsync();
    await act(() => result.current.setConfigOption("model", "claude-sonnet-4-6"));
    expect(result.current.state.pendingConfigOption).toBeNull();
    expect(result.current.state.interactionError?.message).toMatch(error);
  });

  it("is a no-op without a session", async () => {
    const { result } = render("");
    await flushAsync();
    await act(() => result.current.setConfigOption("model", "claude-opus-4-7"));
    expect(calls.filter((c) => c.url.includes("config-option"))).toHaveLength(0);
    expect(result.current.state.pendingConfigOption).toBeNull();
  });

  it("dismisses a switch-failed notice from the stream", async () => {
    const { result } = await openSession("sess-cfg-5");
    lastSocket().message({
      session_id: "sess-cfg-5",
      seq: 1,
      event: { ConfigOptionSwitchFailed: { config_id: "model", value: "claude-sonnet-4-6", reason: "rate limited" } },
    });
    expect(result.current.state.configOptionSwitchFailed?.reason).toBe("rate limited");
    act(() => result.current.dismissConfigOptionSwitchFailed());
    expect(result.current.state.configOptionSwitchFailed).toBeNull();
  });
});

describe("current native admission", () => {
  it.each([
    [false, true, 1],
    [true, false, 0],
  ] as const)(
    "admits %s→%s native readiness from the committed active turn before passive effects (%i cancels)",
    async (before, after, expectedCancels) => {
      const id = "sess-native-commit";
      const admission = Promise.withResolvers<Response>();
      calls = installAcpFakes(({ method, url }) => {
        if (method === "POST" && url.endsWith("/acp/cancel")) return admission.promise;
        if (method === "POST" && url.endsWith("/acp/prompt")) return json({ disposition: "sent" }, 202);
        return undefined;
      });
      let armed: boolean | null = null;
      let gesture: Promise<void> | undefined;
      let gestures = 0;
      let observed: { available: boolean; active: boolean; status: string; seq: number; turn: number } | undefined;
      const hook = renderHook(
        () => {
          const acp = useAcpSession(id);
          const escalation = useCancelEscalation(
            id,
            acp.state.promptSeq,
            acp.state.cancelling,
            acp.cancelPrompt,
            acp.forceEndTurn,
          );
          const { onCancel } = escalation;
          // Host layout follows both real hooks, before their passive effects.
          useLayoutEffect(() => {
            if (armed === null || acp.state.connectionReady !== armed) return;
            armed = null;
            gestures += 1;
            observed = {
              available: nativeInputAvailable(acp.state),
              active: acp.state.turnActive,
              status: acp.status,
              seq: acp.state.lastSeq,
              turn: acp.state.promptSeq,
            };
            gesture = onCancel();
          }, [acp.state, acp.status, onCancel]);
          return { acp, escalation };
        },
        { wrapper: profileWrapper },
      );
      await flushAsync();
      lastSocket().open();
      await flushAsync();
      lastSocket().message(currentControlSnapshot(hook.result.current.acp.state, { ready: true }));
      await flushAsync();
      await act(() => hook.result.current.acp.sendPrompt("a genuinely active turn"));
      lastSocket().message({
        session_id: id,
        seq: 1,
        event: { UserPromptSent: { text: "a genuinely active turn" } },
      });
      await flushAsync();
      expect(posts("/acp/prompt")).toHaveLength(1);
      expect(hook.result.current.acp.state.turnActive).toBe(true);
      expect(hook.result.current.acp.state.serverTurnActive).toBe(true);
      const frontier = hook.result.current.acp.state.lastSeq;
      const turn = hook.result.current.acp.state.promptSeq;
      expect(turn).toBeGreaterThan(0);
      lastSocket().message(currentControlSnapshot(hook.result.current.acp.state, { ready: before }, frontier));
      await flushAsync();
      expect(nativeInputAvailable(hook.result.current.acp.state)).toBe(before);
      armed = after;
      lastSocket().message(currentControlSnapshot(hook.result.current.acp.state, { ready: after }, frontier));
      const forceBeforeAcceptance = hook.result.current.escalation.canForceEndTurn;
      await act(async () => {
        admission.resolve(json({}, 202));
        await gesture;
      });
      expect(gestures).toBe(1);
      expect(observed).toEqual({ available: after, active: true, status: "open", seq: frontier, turn });
      expect(posts("/acp/cancel")).toHaveLength(expectedCancels);
      expect(forceBeforeAcceptance).toBe(false);
      expect(hook.result.current.escalation.canForceEndTurn).toBe(after);
      expect(posts("/acp/force_end_turn")).toEqual([]);
      expect(hook.result.current.acp.state.promptSeq).toBe(turn);
      expect(hook.result.current.acp.state.nativeSnapshotSeq).toBe(frontier);
      hook.unmount();
    },
  );

  it("does not treat socket-open as a native receipt or wake permission", async () => {
    const { result, unmount } = render("sess-open-unknown", "absent", null, "2099-01-01T00:00:00Z");
    await flushAsync();
    lastSocket().open();
    await flushAsync();
    await act(async () => {
      await result.current.sendPrompt("retain this unsent intent");
      await result.current.setConfigOption("model", "other");
      await result.current.cancelPrompt();
    });
    expect(posts("/acp/prompt")).toEqual([]);
    expect(posts("/queue")).toEqual([]);
    expect(posts("/config-option")).toEqual([]);
    expect(posts("/cancel")).toEqual([]);
    expect(calls.filter((c) => c.method === "PATCH" && c.url.endsWith("/snooze"))).toEqual([]);
    expect(result.current.state.optimisticRows.map((row) => row.text)).toContain("retain this unsent intent");
    unmount();
  });

  it("does not wake or queue from generic Unknown, native loss, or a legacy-owner refusal", async () => {
    for (const unavailable of [
      null,
      { source: "native", cause: "stdout_eof", phase: "established" },
      { source: "runner_record", cause: "control_protocol_unknown", phase: "establishing" },
    ] as const) {
      calls = installAcpFakes();
      const { result, unmount } = await openSessionWithReceipt(
        {
          ready: false,
          unavailable,
          wakeEligible: unavailable !== null,
        },
        `sess-refused-${unavailable?.source ?? "unknown"}`,
        "absent",
        null,
        "2099-01-01T00:00:00Z",
      );
      await act(async () => {
        await result.current.sendPrompt("must not wake");
        await result.current.setConfigOption("model", "other");
      });
      expect(posts("/acp/prompt")).toEqual([]);
      expect(posts("/queue")).toEqual([]);
      expect(posts("/config-option")).toEqual([]);
      expect(calls.filter((c) => c.method === "PATCH" && c.url.endsWith("/snooze"))).toEqual([]);
      expect(result.current.canSendQueuedNow).toBe(false);
      unmount();
    }
  });

  it("finishes only its already-authorized snooze intent if its own wake consumes the current eligibility", async () => {
    let queued: ServerQueuedPrompt[] = [];
    calls = installAcpFakes(({ url, method, body }) => {
      if (method === "PATCH" && url.endsWith("/snooze")) {
        lastSocket().message(currentControlSnapshot(emptyAcpState(), { ready: false }));
        return json({});
      }
      if (url.includes("/queue") && method === "GET") return json(queued);
      if (url.includes("/acp/prompt")) {
        queued = [{ id: "governed", seq: 0, text: JSON.parse(body!).text, created_at: "2026-01-01T00:00:00Z" }];
        return json({ disposition: "queued", queued_id: "governed" }, 202);
      }
      return undefined;
    });
    const { result } = await openSessionWithReceipt(
      { ready: false, wakeEligible: true },
      "sess-governed-wake",
      "absent",
      null,
      "2099-01-01T00:00:00Z",
    );
    await act(() => result.current.sendPrompt("one governed intent"));
    expect(posts("/acp/prompt")).toHaveLength(1);
    expect(result.current.state.queuedPrompts.map((row) => row.text)).toEqual(["one governed intent"]);
    await act(() => result.current.setConfigOption("model", "other"));
    expect(posts("/config-option")).toEqual([]);
  });

  it("withdraws the receipt on disconnect without erasing the operation diagnostic", async () => {
    const { result } = await openSession("sess-lost-receipt");
    lastSocket().message({
      session_id: "sess-lost-receipt",
      seq: 1,
      event: { PromptRuntimeError: { message: "canonical request failure" } },
    });
    lastSocket().drop();
    expect(result.current.state.connectionReady).toBe(false);
    expect(result.current.state.nativeSnapshotSeq).toBeNull();
    expect(result.current.state.lastError).toBe("canonical request failure");
  });

  it("retains the existing explicit initial-start queue intent without enabling native controls", async () => {
    calls = installAcpFakes(({ url }) =>
      url.includes("/acp/prompt") ? json({ disposition: "queued", queued_id: "starting-intent" }, 202) : undefined,
    );
    const { result } = await openSessionWithReceipt(
      { ready: false, wakeEligible: true },
      "sess-initial-intent",
      "resuming",
    );
    await act(() => result.current.sendPrompt("initial explicit intent"));
    await act(() => result.current.setConfigOption("model", "other"));
    expect(posts("/acp/prompt")).toHaveLength(1);
    expect(posts("/config-option")).toEqual([]);
    expect(result.current.state.queuedPrompts.map((row) => row.text)).toEqual(["initial explicit intent"]);
    expect(result.current.state.connectionReady).toBe(false);
  });
});

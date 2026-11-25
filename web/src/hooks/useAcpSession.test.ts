// @vitest-environment jsdom

import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { emptyAcpState } from "../lib/acpTypes";
import { STATE_TTL_MS, STORAGE_KEY_PREFIX, getLiveQueueCount } from "../lib/acpStateStorage";
import { listServerQueue, reportAcpInteraction, type ServerQueuedPrompt } from "../lib/api";
import type { SessionResponse } from "../lib/types";
import { cacheGet, cacheSet } from "./acpSession/stateCache";
import { ACP_DIAL_TIMEOUT_MS, ACP_MAX_RETRIES, ACP_WS_STALE_MS, acpRetryDelayMs } from "./acpSession/useAcpConnection";
import {
  FakeWebSocket,
  flushAsync,
  installAcpFakes,
  json,
  lastSocket,
  profileWrapper,
  sockets,
  type Call,
  type Route,
} from "./__tests__/acpHarness";
import { useQueuedCounts } from "./useAcpQueueCount";
import { clearAcpCache, pendingRowIds, pendingRowsChanged, useAcpSession } from "./useAcpSession";

vi.mock("../lib/api", async (importActual) => {
  const actual = await importActual<typeof import("../lib/api")>();
  return { ...actual, reportAcpInteraction: vi.fn(), listServerQueue: vi.fn(actual.listServerQueue) };
});

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

async function openSession(...args: HookArgs) {
  const hook = render(...args);
  await flushAsync();
  lastSocket().open();
  await flushAsync();
  return hook;
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
    for (const socket of sockets) {
      const params = new URL(socket.url).searchParams;
      expect(params.get("projection")).toBe("web");
      expect(params.has("frames")).toBe(false);
    }
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
    const params = new URL(sockets[0]!.url).searchParams;
    expect(params.get("since")).toBe(String(since));
    expect(params.get("projection")).toBe("web");
    expect(params.has("frames")).toBe(false);
  });
});

describe("lightweight control projection", () => {
  it("keeps full background-agent details through reconnect without replayed agent events", async () => {
    const sid = "sess-web-agents";
    let highestSeq = 0;
    calls = installAcpFakes(({ url }) =>
      url.includes("/acp/replay")
        ? json({ frames: [], rows: [], lost: false, highest_seq: highestSeq, next_cursor: highestSeq, has_more: false })
        : undefined,
    );
    const { result } = await openSession(sid);
    expect(new URL(lastSocket().url).searchParams.get("projection")).toBe("web");
    lastSocket().message({
      session_id: sid,
      seq: 10,
      event: {
        BackgroundAgentLaunched: {
          agent_id: "agent-1",
          tool_call_id: "tool-1",
          description: "inspect code",
          prompt: "complete original instructions",
          model: "test-model",
          started_at: "2026-01-01T00:00:00Z",
        },
      },
    });
    const tools = [{ name: "Read", title: "complete/file/path", ok: true }];
    lastSocket().message({
      session_id: sid,
      seq: 11,
      event: {
        BackgroundAgentProgress: {
          agent_id: "agent-1",
          status: "running",
          tool_count: 1,
          tools,
          last_tool: "Read",
          last_text: "latest preview",
        },
      },
    });
    await flushAsync();
    expect(result.current.state.backgroundAgents[0]).toMatchObject({
      prompt: "complete original instructions",
      tools,
      lastText: "latest preview",
      endedAt: null,
    });
    lastSocket().message({
      session_id: sid,
      seq: 12,
      event: {
        BackgroundAgentCompleted: {
          agent_id: "agent-1",
          status: "completed",
          tools,
          result: "complete final report",
          ended_at: "2026-01-01T00:00:01Z",
        },
      },
    });
    highestSeq = 12;
    const webSnapshot = {
      kind: "reduced_state",
      seq: highestSeq,
      state: {
        agent: "claude",
        model: "test-model",
        mode: "default",
        current_plan: null,
        in_flight_tool: null,
        pending_approvals: [],
        pending_elicitations: [],
        thinking: null,
        rate_limit: null,
        available_commands: [],
        available_modes: [],
        current_mode_id: null,
        turn_active: false,
        cancelling: false,
        compacting: false,
      },
    };
    lastSocket().message(webSnapshot);
    await flushAsync();
    expect(result.current.state.backgroundAgents[0]).toMatchObject({
      prompt: "complete original instructions",
      tools,
      status: "completed",
      result: "complete final report",
      endedAt: "2026-01-01T00:00:01Z",
    });
    expect(result.current.state.lastSeq).toBe(highestSeq);
    const agentsBeforeReconnect = result.current.state.backgroundAgents;
    const firstSocket = lastSocket();
    const replayCount = calls.filter((call) => call.url.includes("/acp/replay")).length;

    act(() => result.current.manualReconnect());
    await flushAsync();
    expect(sockets).toHaveLength(2);
    expect(firstSocket.readyState).toBe(FakeWebSocket.CLOSED);
    const params = new URL(lastSocket().url).searchParams;
    expect(params.get("since")).toBe(String(highestSeq));
    expect(params.get("projection")).toBe("web");
    expect(params.has("frames")).toBe(false);
    expect(calls.filter((call) => call.url.includes("/acp/replay")).length).toBeGreaterThan(replayCount);

    lastSocket().open();
    lastSocket().message(webSnapshot);
    await flushAsync();
    expect(result.current.status).toBe("open");
    expect(result.current.state.lastSeq).toBe(highestSeq);
    expect(result.current.state.backgroundAgents).toEqual(agentsBeforeReconnect);
  });
});

describe("mid-turn cache recovery (#4530)", () => {
  it("keeps the cached window through delayed catch-up and reconnect without new events", async () => {
    const sid = "sess-stable-warm";
    const rows = Array.from({ length: 6 }, (_, index) => [
      { id: `user-${index}`, kind: "user_prompt" as const, text: `prompt ${index}`, at: "t" },
      { id: `reply-${index}`, kind: "message" as const, text: `reply ${index}`, at: "t" },
    ]).flat();
    localStorage.setItem(
      `${STORAGE_KEY_PREFIX}${sid}`,
      JSON.stringify({
        savedAt: Date.now(),
        state: { ...emptyAcpState(), lastSeq: 1200, oldestSeq: 1, activity: rows },
      }),
    );
    let release!: () => void;
    const held = new Promise<void>((resolve) => {
      release = resolve;
    });
    let reads = 0;
    calls = installAcpFakes(async ({ url }) => {
      if (!url.includes("/acp/replay")) return undefined;
      reads += 1;
      await held;
      const params = new URL(url, "http://x").searchParams;
      let incoming: typeof rows = [];
      if (params.get("view") === "rows") incoming = [{ ...rows[rows.length - 1]!, text: "canonical reply" }];
      return json({ frames: [], rows: incoming, lost: false, highest_seq: 1200, next_cursor: 1200, has_more: false });
    });
    const renders: string[][] = [];
    const { result, unmount } = renderHook(
      () => {
        const session = useAcpSession(sid);
        renders.push(session.state.activity.map((row) => row.id));
        return session;
      },
      { wrapper: profileWrapper },
    );
    await waitFor(() => expect(reads).toBe(2), { timeout: 2000 });
    expect(result.current.state.activity).toEqual(rows);
    await act(async () => release());
    await waitFor(() => expect(result.current.state.activity.at(-1)?.text).toBe("canonical reply"), { timeout: 2000 });
    await waitFor(() => expect(sockets).toHaveLength(1), { timeout: 2000 });
    lastSocket().open();
    expect(result.current.state.activity.map((row) => row.id)).toEqual(rows.map((row) => row.id));
    const firstSocket = lastSocket();
    act(() => result.current.manualReconnect());
    await waitFor(() => expect(reads).toBe(4), { timeout: 2000 });
    await waitFor(() => expect(sockets).toHaveLength(2), { timeout: 2000 });
    lastSocket().open();
    lastSocket().message({ kind: "transcript_snapshot", rows: [] });
    expect(firstSocket.readyState).toBe(FakeWebSocket.CLOSED);
    expect(result.current.state.activity.map((row) => row.id)).toEqual(rows.map((row) => row.id));
    expect(result.current.state.oldestSeq).toBe(1);
    expect(new URL(lastSocket().url).searchParams.get("since")).toBe("1200");
    for (const ids of renders) expect(ids).toEqual(rows.map((row) => row.id));
    unmount();
  });

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

describe("persist flush on cleanup", () => {
  const storedKey = (id: string) => `${STORAGE_KEY_PREFIX}${id}`;
  const seed = (id: string) => cacheSet(id, { ...emptyAcpState(), lastSeq: 7 });

  it("writes pending state on unmount, before the throttle expires", async () => {
    installAcpFakes();
    seed("sess-unmount");
    const { unmount } = render("sess-unmount");
    await flushAsync();
    expect(localStorage.getItem(storedKey("sess-unmount"))).toBeNull();
    unmount();
    expect(JSON.parse(localStorage.getItem(storedKey("sess-unmount"))!).state.lastSeq).toBe(7);
  });

  it("persists a queue confirmation at once, so another window cannot read the row as unconfirmed", async () => {
    const id = "sess-confirm";
    const queuedAt = "2026-01-01T00:00:00.000Z";
    cacheSet(id, { ...emptyAcpState(), queuedPrompts: [{ id: "u1", text: "u1", queuedAt, pending: true }] });
    const hook = render(id, "running");
    await flushAsync();
    lastSocket().open();
    await waitFor(() => expect(hook.result.current.state.queuedPrompts).toMatchObject([{ id: "u1", pending: false }]));
    expect(JSON.parse(localStorage.getItem(storedKey(id))!).state.queuedPrompts).toMatchObject([
      { id: "u1", pending: false },
    ]);
    hook.unmount();
  });

  it("writes the previous session's pending state on a session switch", async () => {
    installAcpFakes();
    seed("sess-a");
    const { rerender } = renderHook(({ id }: { id: string }) => useAcpSession(id), {
      initialProps: { id: "sess-a" },
      wrapper: profileWrapper,
    });
    await flushAsync();
    expect(localStorage.getItem(storedKey("sess-a"))).toBeNull();
    rerender({ id: "sess-b" });
    expect(JSON.parse(localStorage.getItem(storedKey("sess-a"))!).state.lastSeq).toBe(7);
  });
});

describe("pendingRowsChanged", () => {
  const queuedAt = "2026-01-01T00:00:00.000Z";
  const row = (id: string, pending?: boolean) => ({ id, text: id, queuedAt, pending });
  const withRows = (...rows: ReturnType<typeof row>[]) => ({ ...emptyAcpState(), queuedPrompts: rows });

  it("flags an enqueue, and a confirmation by row identity even when another row is enqueued in the same render", () => {
    const before = pendingRowIds(withRows(row("a", true)));
    expect(pendingRowsChanged(before, withRows(row("a", false), row("b", true)))).toBe(true);
    expect(pendingRowsChanged(before, withRows(row("a", true), row("b", true)))).toBe(true);
    expect(pendingRowsChanged(before, withRows())).toBe(true);
    expect(pendingRowsChanged(before, withRows(row("a", true)))).toBe(false);
    expect(pendingRowsChanged(before, withRows(row("a", true), row("c", false)))).toBe(false);
  });
});

describe("sendPrompt outcomes", () => {
  it("identifies the dashboard as the prompt's origin", async () => {
    calls = installAcpFakes(({ url }) =>
      url.includes("/acp/prompt") ? json({ disposition: "sent" }, 202) : undefined,
    );
    const { result, unmount } = await openSession("s-1");
    await act(() => result.current.sendPrompt("hello"));
    expect(posts("/acp/prompt")[0]!.headers["X-Aoe-Prompt-Origin"]).toBe("dashboard");
    unmount();
  });

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
      const { result } = render("sess-wake", "absent", null, "2099-01-01T00:00:00Z");
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
      expect(result.current.state.lastError).toBe("session is archived; unarchive it first");
    });

    it("does not call wake endpoints for a live session", async () => {
      calls = installAcpFakes(queuedPrompt);
      const { result } = render("sess-live", "absent", null, null);
      await flushAsync();
      await act(() => result.current.sendPrompt("just a prompt"));
      await flushAsync();
      expect(calls.filter((c) => c.method === "PATCH")).toHaveLength(0);
      expect(result.current.state.queuedPrompts).toHaveLength(1);
    });

    it("sends nothing when the snooze wake fails", async () => {
      calls = installAcpFakes(failingWake("/snooze"));
      const { result } = render("sess-wake-fail", "absent", null, "2099-01-01T00:00:00Z");
      await flushAsync();
      await act(() => result.current.sendPrompt("wake me up"));
      await flushAsync();
      expect(posts("/acp/prompt")).toHaveLength(0);
      expect(result.current.state.queuedPrompts).toHaveLength(0);
      expect(result.current.state.lastError).toMatch(/wake/i);
    });
  });

  // The daemon kept the prompt for the sandbox login: a queue row, not a rejected send, and the
  // banner says what to run.
  it("treats a 409 login-required prompt with a queued_id as queued and shows the login guidance", async () => {
    const message = `sandbox login required for claude: run \`aoe sandbox login --tool claude\``;
    calls = installAcpFakes(({ url }) =>
      url.includes("/acp/prompt")
        ? json(
            {
              error: "sandbox_login_required",
              message,
              queued_id: "0b6f1c1e-3c55-4b8e-9a51-2f3d4c5b6a79",
            },
            409,
          )
        : undefined,
    );
    const { result } = render("sess-login", "absent", null, null);
    await flushAsync();
    await act(() => result.current.sendPrompt("after the login"));
    await flushAsync();
    expect(result.current.state.queuedPrompts.map((q) => q.text)).toEqual(["after the login"]);
    expect(result.current.state.lastError).toBeNull();
    expect(result.current.state.startupError).toBe(message);
  });

  // A failing `before_start` hook: the daemon kept the prompt (502 with a queue id). It is a queue
  // row, not a rejected send, so a retry does not enqueue it twice, and the banner says so in plain
  // words without the daemon's text.
  it("treats a 502 before_start_hook_failed prompt with a queued_id as queued, with a plain message", async () => {
    calls = installAcpFakes(({ url }) =>
      url.includes("/acp/prompt")
        ? json(
            {
              error: "before_start_hook_failed",
              message: "the before_start hook failed",
              queued_id: "5d1f0a52-9e0b-4a47-8d0e-6f1b2c3d4e5f",
            },
            502,
          )
        : undefined,
    );
    const { result } = render("sess-hook-failed", "absent", null, null);
    await flushAsync();
    await act(() => result.current.sendPrompt("once the hook works"));
    await flushAsync();
    expect(result.current.state.queuedPrompts.map((q) => q.text)).toEqual(["once the hook works"]);
    expect(result.current.state.lastError).toBeNull();
    expect(result.current.state.startupError).toMatch(/before_start hook failed; your message is queued/);
    expect(result.current.state.startupError).not.toMatch(/502|\{/);
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
    const { result } = render("sess-capacity", "running", null, null);
    await flushAsync();
    await act(() => result.current.sendPrompt("no room at the inn"));
    await flushAsync();
    expect(posts("/queue")).toHaveLength(0);
    expect(result.current.state.lastError).toMatch(/capacity/i);
  });

  // A newer stop refused the prompt's start: a non-retryable 409, so the prompt is rejected with
  // its banner and is never re-queued, where it could be delivered after a later start.
  it("rejects a 409 resume_failed prompt without queueing it", async () => {
    calls = installAcpFakes(({ url, method }) => {
      if (url.includes("/acp/replay")) return json({ frames: [], lost: false, highest_seq: 0 });
      if (url.includes("/acp/prompt")) {
        return new Response("stop_fenced: the session was stopped again; start it to send this message", {
          status: 409,
          headers: { "AoE-Error-Code": "resume_failed" },
        });
      }
      if (url.includes("/queue") && method === "GET") return json([]);
      return undefined;
    });
    const { result } = render("sess-stop-fenced", "absent", null, null);
    await flushAsync();
    await act(() => result.current.sendPrompt("too late"));
    await flushAsync();
    expect(posts("/queue")).toHaveLength(0);
    expect(result.current.state.lastError).toMatch(/409.*stop_fenced/);
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

    // The re-queue keys on the daemon's `worker_not_ready`, not on this flag: a park the client
    // has not learned about yet must not cost the prompt, so BOTH rows re-enqueue. The flag is
    // still asserted because `canSendQueuedNow` reads it.
    it.each([[true], [false]])("parked=%s re-enqueues a 503'd prompt", async (isParked) => {
      calls = installAcpFakes(parked(isParked));
      const { result } = render(`sess-${tag}`, "running", null, null);
      await flushAsync();
      expect(result.current.state[flag]).toBe(isParked);
      await act(() => result.current.sendPrompt("try again after the park"));
      await flushAsync();
      expect(posts("/queue")).toHaveLength(1);
      expect(result.current.state.queuedPrompts.map((q) => q.text)).toEqual(["try again after the park"]);
    });

    it("offers Send now on a row stranded behind the park", async () => {
      installAcpFakes(parked(true));
      const { result } = await openSession(`sess-${tag}-send`, "absent", null, null);
      expect(result.current.status).toBe("open");
      expect(result.current.canSendQueuedNow).toBe(true);
    });
  });
});

describe("server queue", () => {
  let serverQueue: Map<string, ServerQueuedPrompt>;
  let serverBusy: boolean;
  let workerDown: boolean;
  // Set to hold the queue list's response until it resolves.
  let queueListHeld: Promise<void> | null;
  let queueListStarted: (() => void) | null;
  let queueListFails: boolean;
  // Set to hold each queue POST's response until it resolves.
  let queuePostHeld: Promise<void> | null;
  let queuePostStarted: (() => void) | null;
  // Rows the Daemon delivers as soon as their first POST commits.
  let deliveredOnPost: Set<string>;
  // Holds a POST before it commits, so a DELETE meanwhile finds nothing.
  let queuePostUncommitted: Promise<void> | null;
  // Holds a DELETE of one row before it is applied.
  let queueDeleteHeld: Promise<void> | null;
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
    queueListHeld = null;
    queueListStarted = null;
    queueListFails = false;
    queuePostHeld = null;
    queuePostStarted = null;
    deliveredOnPost = new Set();
    queuePostUncommitted = null;
    queueDeleteHeld = null;
    vi.mocked(reportAcpInteraction).mockClear();
    vi.mocked(listServerQueue).mockClear();
    calls = installAcpFakes(({ url, method, body }) => {
      if (url.includes("/queue")) {
        const item = url.match(/\/queue\/([^/?]+)/)?.[1];
        if (method === "GET") {
          queueListStarted?.();
          if (queueListFails) return new Response("unavailable", { status: 503 });
          const list = () => json([...serverQueue.values()].sort((a, b) => a.seq - b.seq));
          return queueListHeld ? queueListHeld.then(list) : list();
        }
        if (method === "POST" && queuePostUncommitted) {
          const parsed = JSON.parse(body!) as { id: string; text: string };
          const held = queuePostUncommitted;
          queuePostStarted?.();
          return held.then(() => {
            serverQueue.set(parsed.id, row(parsed.id, parsed.text));
            return json(serverQueue.get(parsed.id));
          });
        }
        if (method === "POST") {
          const parsed = JSON.parse(body!) as { id: string; text: string };
          serverQueue.set(parsed.id, row(parsed.id, parsed.text));
          queuePostStarted?.();
          const saved = serverQueue.get(parsed.id);
          if (deliveredOnPost.delete(parsed.id)) serverQueue.delete(parsed.id);
          return queuePostHeld ? queuePostHeld.then(() => json(saved)) : json(saved);
        }
        if (method === "PATCH" && item) {
          const existing = serverQueue.get(decodeURIComponent(item));
          if (!existing) return new Response("queued prompt not found", { status: 404 });
          existing.text = (JSON.parse(body!) as { text: string }).text;
          return new Response(null, { status: 204 });
        }
        if (method === "DELETE" && item && queueDeleteHeld) {
          return queueDeleteHeld.then(() =>
            serverQueue.delete(decodeURIComponent(item))
              ? new Response(null, { status: 204 })
              : new Response("queued prompt not found", { status: 404 }),
          );
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

  it("lends the sidebar its queue count only from the queue list after an open until the close", async () => {
    const id = "sess-live-count";
    const persisted = ["p1", "p2", "p3"].map((pid) => ({ id: pid, text: pid, queuedAt: "2026-01-01T00:00:00.000Z" }));
    localStorage.setItem(
      `${STORAGE_KEY_PREFIX}${id}`,
      JSON.stringify({ savedAt: Date.now(), state: { ...emptyAcpState(), queuedPrompts: persisted } }),
    );
    serverQueue.set("q1", row("q1", "held"));
    const hook = render(id);
    await flushAsync();
    // The persisted copy is on screen but isn't the Daemon's queue.
    expect(hook.result.current.state.queuedPrompts).toHaveLength(3);
    expect(getLiveQueueCount(id)).toBeUndefined();

    const list = Promise.withResolvers<void>();
    const started = Promise.withResolvers<void>();
    queueListHeld = list.promise;
    queueListStarted = started.resolve;
    lastSocket().open();
    await started.promise;
    // Open, but the Daemon's list hasn't arrived: the persisted copy still isn't lent.
    expect(getLiveQueueCount(id)).toBeUndefined();
    queueListHeld = null;
    list.resolve();
    await waitFor(() => expect(getLiveQueueCount(id)).toBe(1));

    // A row queued here counts before any session list could report it.
    serverBusy = true;
    lastSocket().message({ session_id: id, seq: 1, event: { UserPromptSent: { text: "kick" } } });
    await flushAsync();
    act(() => void hook.result.current.sendPrompt("follow-up"));
    await flushAsync();
    expect(getLiveQueueCount(id)).toBe(2);

    lastSocket().drop();
    await flushAsync();
    expect(getLiveQueueCount(id)).toBeUndefined();

    lastSocket().open();
    await flushAsync();
    expect(getLiveQueueCount(id)).toBe(2);
    hook.unmount();
    expect(getLiveQueueCount(id)).toBeUndefined();
  });

  it("falls back to the Daemon's count when a queue list fails, and re-lists when the polled count changes", async () => {
    const id = "sess-polled";
    const persisted = ["p1", "p2"].map((pid) => ({ id: pid, text: pid, queuedAt: "2026-01-01T00:00:00.000Z" }));
    localStorage.setItem(
      `${STORAGE_KEY_PREFIX}${id}`,
      JSON.stringify({ savedAt: Date.now(), state: { ...emptyAcpState(), queuedPrompts: persisted } }),
    );
    serverQueue.set("q1", row("q1", "first"));
    queueListFails = true;
    const hook = renderHook(
      ({ polled }: { polled: number }) => ({
        acp: useAcpSession(id, "running", null, null, polled),
        chip: useQueuedCounts([{ id, queued_count: polled } as SessionResponse]),
      }),
      { wrapper: profileWrapper, initialProps: { polled: 1 } },
    );
    await flushAsync();
    lastSocket().open();
    await waitFor(() => expect(listServerQueue).toHaveBeenCalledTimes(1));
    // The hook's continuation was registered on this promise first, so it has run once this settles.
    await act(async () => {
      await vi.mocked(listServerQueue).mock.results[0]!.value;
    });
    expect(getLiveQueueCount(id)).toBeUndefined();
    expect(hook.result.current.chip).toEqual([1]);
    // The rows on screen are still the persisted copy, so changing them lends no count either.
    expect(hook.result.current.acp.state.queuedPrompts.map((q) => q.id)).toEqual(["p1", "p2"]);
    act(() => hook.result.current.acp.removeQueuedPrompt("p1"));
    expect(getLiveQueueCount(id)).toBeUndefined();
    expect(hook.result.current.chip).toEqual([1]);

    // Another tab queues a row; the next poll reports it and this tab re-lists.
    queueListFails = false;
    serverQueue.set("q2", row("q2", "from another tab"));
    hook.rerender({ polled: 2 });
    await waitFor(() => expect(getLiveQueueCount(id)).toBe(2));
    expect(hook.result.current.chip).toEqual([2]);
    expect(hook.result.current.acp.state.queuedPrompts.map((q) => q.text)).toEqual(["first", "from another tab"]);

    // A refresh that fails after a list succeeded stops lending this tab's now stale count.
    queueListFails = true;
    serverQueue.set("q3", row("q3", "third"));
    hook.rerender({ polled: 3 });
    await waitFor(() => expect(getLiveQueueCount(id)).toBeUndefined());
    expect(hook.result.current.chip).toEqual([3]);
    expect(hook.result.current.acp.state.queuedPrompts.map((q) => q.text)).toEqual(["first", "from another tab"]);
    queueListFails = false;
    hook.rerender({ polled: 4 });
    await waitFor(() => expect(getLiveQueueCount(id)).toBe(3));
    hook.unmount();
  });

  it("re-sends each unconfirmed row once when the polled count changes mid-migration, and drops one delivered meanwhile", async () => {
    const id = "sess-migrating";
    const queuedAt = "2026-01-01T00:00:00.000Z";
    const state = {
      ...emptyAcpState(),
      queuedPrompts: ["u1", "u2", "u3"].map((pid) => ({ id: pid, text: pid, queuedAt, pending: true })),
    };
    localStorage.setItem(`${STORAGE_KEY_PREFIX}${id}`, JSON.stringify({ savedAt: Date.now(), state }));
    const hook = renderHook(({ polled }: { polled: number }) => useAcpSession(id, "running", null, null, polled), {
      wrapper: profileWrapper,
      initialProps: { polled: 0 },
    });
    await flushAsync();
    const post = Promise.withResolvers<void>();
    const started = Promise.withResolvers<void>();
    queuePostHeld = post.promise;
    queuePostStarted = started.resolve;
    lastSocket().open();
    await started.promise;
    // The first POST commits, which moves the poll, and the Daemon delivers it before its response arrives.
    hook.rerender({ polled: 1 });
    await flushAsync();
    serverQueue.delete("u1");
    queuePostHeld = null;
    post.resolve();
    await waitFor(() => expect(hook.result.current.state.queuedPrompts.map((q) => q.id)).toEqual(["u2", "u3"]));
    expect(posts("/queue").map((c) => JSON.parse(c.body!).id)).toEqual(["u1", "u2", "u3"]);
    expect(serverQueue.has("u1")).toBe(false);
    expect(hook.result.current.state.queuedPrompts.every((q) => !q.pending)).toBe(true);
    hook.unmount();
  });

  it("retries a failed queue list a bounded number of times, and stops on success or cleanup", async () => {
    vi.useFakeTimers();
    const id = "sess-list-retry";
    const lists = () => calls.filter((c) => c.method === "GET" && c.url.includes("/queue")).length;
    queueListFails = true;
    const hook = renderHook(({ polled }: { polled: number }) => useAcpSession(id, "running", null, null, polled), {
      wrapper: profileWrapper,
      initialProps: { polled: 1 },
    });
    await flushAsync();
    lastSocket().open();
    await flushAsync();
    expect(lists()).toBe(1);
    // The server recovers while the poll keeps reporting the same count.
    await act(() => vi.advanceTimersByTimeAsync(1999));
    expect(lists()).toBe(1);
    await act(() => vi.advanceTimersByTimeAsync(1));
    expect(lists()).toBe(2);
    queueListFails = false;
    serverQueue.set("q1", row("q1", "recovered"));
    await act(() => vi.advanceTimersByTimeAsync(5000));
    expect(lists()).toBe(3);
    expect(hook.result.current.state.queuedPrompts.map((q) => q.text)).toEqual(["recovered"]);
    expect(getLiveQueueCount(id)).toBe(1);
    // Past every delay, but short of the socket's staleness timeout.
    await act(() => vi.advanceTimersByTimeAsync(20_000));
    expect(lists()).toBe(3);

    // Failing every time, it gives up after its last delay until the next trigger.
    queueListFails = true;
    hook.rerender({ polled: 2 });
    await flushAsync();
    expect(lists()).toBe(4);
    await act(() => vi.advanceTimersByTimeAsync(2000 + 5000 + 15000));
    expect(lists()).toBe(7);
    await act(() => vi.advanceTimersByTimeAsync(20_000));
    expect(lists()).toBe(7);

    // A pending retry ends with the effect.
    hook.rerender({ polled: 3 });
    await flushAsync();
    expect(lists()).toBe(8);
    hook.unmount();
    await act(() => vi.advanceTimersByTimeAsync(60_000));
    expect(lists()).toBe(8);
  });

  it.each([
    ["opens another Session", "switch"],
    ["closes it", "unmount"],
  ] as const)("confirms a re-sent row in its Session's stored state when the user %s mid-POST", async (_, leave) => {
    const a = `sess-left-${leave}`;
    const queuedAt = "2026-01-01T00:00:00.000Z";
    const state = {
      ...emptyAcpState(),
      queuedPrompts: [{ id: "u1", text: "u1", queuedAt, pending: true }],
    };
    localStorage.setItem(`${STORAGE_KEY_PREFIX}${a}`, JSON.stringify({ savedAt: Date.now(), state }));
    const first = renderHook(({ id }: { id: string }) => useAcpSession(id, "running"), {
      wrapper: profileWrapper,
      initialProps: { id: a },
    });
    await flushAsync();
    const post = Promise.withResolvers<void>();
    const started = Promise.withResolvers<void>();
    queuePostHeld = post.promise;
    queuePostStarted = started.resolve;
    lastSocket().open();
    await started.promise;
    // The user leaves A; the Daemon then delivers u1 before its response arrives.
    if (leave === "switch") first.rerender({ id: "sess-elsewhere" });
    else first.unmount();
    await flushAsync();
    serverQueue.delete("u1");
    queuePostHeld = null;
    post.resolve();
    await waitFor(() => expect(cacheGet(a)?.queuedPrompts).toMatchObject([{ id: "u1", pending: false }]));
    if (leave === "switch") first.unmount();

    const lists = () => calls.filter((c) => c.method === "GET" && c.url.includes("/queue") && c.url.includes(a)).length;
    const listed = lists();
    const again = render(a);
    await flushAsync();
    lastSocket().open();
    await waitFor(() => expect(lists()).toBeGreaterThan(listed));
    await waitFor(() => expect(again.result.current.state.queuedPrompts).toEqual([]));
    expect(posts("/queue").map((c) => JSON.parse(c.body!).id)).toEqual(["u1"]);
    expect(serverQueue.has("u1")).toBe(false);
    again.unmount();
  });

  it("runs one re-send per Session across a close and reopen mid-POST, so a delivered row isn't posted again", async () => {
    const a = "sess-remount";
    const queuedAt = "2026-01-01T00:00:00.000Z";
    const pending = ["A", "B", "C"].map((id) => ({ id, text: id, queuedAt, pending: true }));
    const state = { ...emptyAcpState(), queuedPrompts: pending };
    localStorage.setItem(`${STORAGE_KEY_PREFIX}${a}`, JSON.stringify({ savedAt: Date.now(), state }));
    const postIds = () => posts("/queue").map((c) => JSON.parse(c.body!).id as string);
    const lists = () => calls.filter((c) => c.method === "GET" && c.url.includes("/queue") && c.url.includes(a)).length;
    deliveredOnPost.add("B");
    const first = render(a);
    await flushAsync();
    const post = Promise.withResolvers<void>();
    const started = Promise.withResolvers<void>();
    queuePostHeld = post.promise;
    queuePostStarted = started.resolve;
    lastSocket().open();
    await started.promise;
    queuePostHeld = null;
    queuePostStarted = null;
    // The user closes and reopens A while A's first POST is held.
    first.unmount();
    const again = render(a);
    await flushAsync();
    lastSocket().open();
    await flushAsync();
    const listed = lists();
    post.resolve();
    // The reopened hook lists only after every re-send for A has settled.
    await waitFor(() => expect(lists()).toBeGreaterThan(listed));
    const shown = () => again.result.current.state.queuedPrompts;
    await waitFor(() => expect(shown().map((q) => q.id)).toEqual(["A", "C"]));
    expect(shown().some((q) => q.pending)).toBe(false);
    expect(postIds()).toEqual(["A", "B", "C"]);
    expect([...serverQueue.keys()]).toEqual(["A", "C"]);
    again.unmount();
  });

  it.each([
    ["opens another Session", "switch"],
    ["closes it", "unmount"],
  ] as const)("confirms a prompt queued while open in its stored state when the user %s mid-POST", async (_, leave) => {
    const a = `sess-queued-${leave}`;
    const lists = () => calls.filter((c) => c.method === "GET" && c.url.includes("/queue") && c.url.includes(a)).length;
    const first = renderHook(({ id }: { id: string }) => useAcpSession(id, "running"), {
      wrapper: profileWrapper,
      initialProps: { id: a },
    });
    await flushAsync();
    lastSocket().open();
    await flushAsync();
    const post = Promise.withResolvers<void>();
    const started = Promise.withResolvers<void>();
    queuePostHeld = post.promise;
    queuePostStarted = started.resolve;
    workerDown = true;
    await act(() => first.result.current.sendPrompt("later"));
    await started.promise;
    queuePostHeld = null;
    queuePostStarted = null;
    workerDown = false;
    // The user leaves A; the Daemon then delivers the row before its response arrives.
    if (leave === "switch") first.rerender({ id: "sess-elsewhere" });
    else first.unmount();
    await flushAsync();
    serverQueue.clear();
    post.resolve();
    await waitFor(() => expect(cacheGet(a)?.queuedPrompts).toMatchObject([{ text: "later", pending: false }]));
    if (leave === "switch") first.unmount();

    const listed = lists();
    const again = render(a);
    await flushAsync();
    lastSocket().open();
    await waitFor(() => expect(lists()).toBeGreaterThan(listed));
    await waitFor(() => expect(again.result.current.state.queuedPrompts).toEqual([]));
    expect(posts("/queue")).toHaveLength(1);
    expect(serverQueue.size).toBe(0);
    again.unmount();
  });

  it.each([
    ["removes B", "remove", ["A", "C"]],
    ["clears the queue", "clear", []],
  ] as const)("posts no row the user %s while an earlier re-send POST is held", async (_, action, kept) => {
    const a = `sess-removed-${action}`;
    const queuedAt = "2026-01-01T00:00:00.000Z";
    const pending = ["A", "B", "C"].map((id) => ({ id, text: id, queuedAt, pending: true }));
    localStorage.setItem(
      `${STORAGE_KEY_PREFIX}${a}`,
      JSON.stringify({ savedAt: Date.now(), state: { ...emptyAcpState(), queuedPrompts: pending } }),
    );
    const postIds = () => posts("/queue").map((c) => JSON.parse(c.body!).id as string);
    const lists = () => calls.filter((c) => c.method === "GET" && c.url.includes("/queue") && c.url.includes(a)).length;
    const hook = render(a);
    await flushAsync();
    const post = Promise.withResolvers<void>();
    const started = Promise.withResolvers<void>();
    queuePostHeld = post.promise;
    queuePostStarted = started.resolve;
    lastSocket().open();
    await started.promise;
    queuePostHeld = null;
    queuePostStarted = null;
    act(() => (action === "remove" ? hook.result.current.removeQueuedPrompt("B") : hook.result.current.clearQueue()));
    const listed = lists();
    post.resolve();
    await waitFor(() => expect(lists()).toBeGreaterThan(listed));
    await waitFor(() => expect(hook.result.current.state.queuedPrompts.map((q) => q.id)).toEqual(kept));
    expect(postIds()).toEqual(["A", ...kept.filter((id) => id !== "A")]);
    expect([...serverQueue.keys()]).toEqual(kept);
    hook.unmount();
  });

  it("deletes a re-sent row again when the user removes it while its POST is in flight", async () => {
    const a = "sess-removed-in-flight";
    const queuedAt = "2026-01-01T00:00:00.000Z";
    localStorage.setItem(
      `${STORAGE_KEY_PREFIX}${a}`,
      JSON.stringify({
        savedAt: Date.now(),
        state: { ...emptyAcpState(), queuedPrompts: [{ id: "B", text: "B", queuedAt, pending: true }] },
      }),
    );
    const deletes = () => calls.filter((c) => c.method === "DELETE" && c.url.includes("/queue/B")).length;
    const lists = () => calls.filter((c) => c.method === "GET" && c.url.includes("/queue") && c.url.includes(a)).length;
    const hook = render(a);
    await flushAsync();
    const commit = Promise.withResolvers<void>();
    const started = Promise.withResolvers<void>();
    queuePostUncommitted = commit.promise;
    queuePostStarted = started.resolve;
    lastSocket().open();
    await started.promise;
    queuePostUncommitted = null;
    queuePostStarted = null;
    // The DELETE reaches the server before B's POST commits, so it finds nothing.
    act(() => hook.result.current.removeQueuedPrompt("B"));
    await waitFor(() => expect(deletes()).toBe(1));
    const listed = lists();
    const redelete = Promise.withResolvers<void>();
    queueDeleteHeld = redelete.promise;
    commit.resolve();
    await waitFor(() => expect(deletes()).toBe(2));
    // The list after the re-send waits for that DELETE, so it can't show B again.
    await flushAsync();
    expect(lists()).toBe(listed);
    queueDeleteHeld = null;
    redelete.resolve();
    await waitFor(() => expect(lists()).toBeGreaterThan(listed));
    expect(serverQueue.has("B")).toBe(false);
    expect(hook.result.current.state.queuedPrompts).toEqual([]);
    expect(posts("/queue").map((c) => JSON.parse(c.body!).id)).toEqual(["B"]);
    hook.unmount();
  });

  it.each([
    ["before its POST runs", "before"],
    ["while its POST is in flight", "in-flight"],
  ] as const)("re-sends the latest text of a row the user edits %s", async (_, when) => {
    const a = `sess-edited-${when}`;
    const queuedAt = "2026-01-01T00:00:00.000Z";
    const pending = ["A", "B"].map((id) => ({ id, text: id, queuedAt, pending: true }));
    localStorage.setItem(
      `${STORAGE_KEY_PREFIX}${a}`,
      JSON.stringify({ savedAt: Date.now(), state: { ...emptyAcpState(), queuedPrompts: pending } }),
    );
    const lists = () => calls.filter((c) => c.method === "GET" && c.url.includes("/queue") && c.url.includes(a)).length;
    const hook = render(a);
    await flushAsync();
    const release = Promise.withResolvers<void>();
    const started = Promise.withResolvers<void>();
    // Before: A's POST is held after it commits. In flight: B's own POST is held before it commits,
    // so the edit's PATCH finds nothing.
    if (when === "before") queuePostHeld = release.promise;
    else {
      queuePostStarted = () => {
        if (posts("/queue").length === 1) queuePostUncommitted = release.promise;
      };
    }
    const bStarted = () => posts("/queue").some((c) => JSON.parse(c.body!).id === "B");
    if (when === "before") queuePostStarted = started.resolve;
    lastSocket().open();
    if (when === "before") await started.promise;
    else await waitFor(() => expect(bStarted()).toBe(true));
    queuePostHeld = null;
    queuePostStarted = null;
    queuePostUncommitted = null;
    act(() => hook.result.current.editQueuedPrompt("B", "B edited"));
    await waitFor(() => expect(calls.some((c) => c.method === "PATCH")).toBe(true));
    const listed = lists();
    release.resolve();
    await waitFor(() => expect(lists()).toBeGreaterThan(listed));
    expect(serverQueue.get("B")?.text).toBe("B edited");
    // An edit before the POST runs is posted as is; one in flight is patched after the save.
    const postedB = posts("/queue").filter((c) => JSON.parse(c.body!).id === "B");
    expect(postedB.map((c) => JSON.parse(c.body!).text)).toEqual([when === "before" ? "B edited" : "B"]);
    expect(hook.result.current.state.queuedPrompts.map((q) => q.text)).toEqual(["A", "B edited"]);
    hook.unmount();
  });

  it("deletes a prompt queued while open again when the user removes it before its POST commits", async () => {
    const a = "sess-queued-removed";
    const lists = () => calls.filter((c) => c.method === "GET" && c.url.includes("/queue") && c.url.includes(a)).length;
    const hook = await openSession(a);
    const commit = Promise.withResolvers<void>();
    const started = Promise.withResolvers<void>();
    queuePostUncommitted = commit.promise;
    queuePostStarted = started.resolve;
    workerDown = true;
    await act(() => hook.result.current.sendPrompt("later"));
    await started.promise;
    queuePostUncommitted = null;
    queuePostStarted = null;
    workerDown = false;
    const id = hook.result.current.state.queuedPrompts[0]!.id;
    const deletes = () => calls.filter((c) => c.method === "DELETE" && c.url.includes(`/queue/${id}`)).length;
    act(() => hook.result.current.removeQueuedPrompt(id));
    await waitFor(() => expect(deletes()).toBe(1));
    commit.resolve();
    await waitFor(() => expect(deletes()).toBe(2));
    expect(serverQueue.has(id)).toBe(false);
    // The next list doesn't bring it back.
    const listed = lists();
    hook.rerender();
    act(() => hook.result.current.manualReconnect());
    await flushAsync();
    lastSocket().open();
    await waitFor(() => expect(lists()).toBeGreaterThan(listed));
    expect(hook.result.current.state.queuedPrompts).toEqual([]);
    hook.unmount();
  });

  it("confirms a re-sent row in the open hook under StrictMode, so a delivered one is dropped", async () => {
    const a = "sess-strict";
    const queuedAt = "2026-01-01T00:00:00.000Z";
    const state = {
      ...emptyAcpState(),
      queuedPrompts: [{ id: "u1", text: "u1", queuedAt, pending: true }],
    };
    localStorage.setItem(`${STORAGE_KEY_PREFIX}${a}`, JSON.stringify({ savedAt: Date.now(), state }));
    const hook = renderHook(() => useAcpSession(a, "running"), { wrapper: profileWrapper, reactStrictMode: true });
    await flushAsync();
    const post = Promise.withResolvers<void>();
    const started = Promise.withResolvers<void>();
    queuePostHeld = post.promise;
    queuePostStarted = started.resolve;
    lastSocket().open();
    await started.promise;
    serverQueue.delete("u1");
    queuePostHeld = null;
    const lists = () => calls.filter((c) => c.method === "GET" && c.url.includes("/queue") && c.url.includes(a)).length;
    const listed = lists();
    post.resolve();
    await waitFor(() => expect(lists()).toBeGreaterThan(listed));
    await waitFor(() => expect(hook.result.current.state.queuedPrompts).toEqual([]));
    expect(posts("/queue").map((c) => JSON.parse(c.body!).id)).toEqual(["u1"]);
    hook.unmount();
  });

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
    });
  });
});

describe("retryRejectedPrompt (#5662)", () => {
  const image = { id: "att-1", kind: "image" as const, mime_type: "image/png", name: "shot.png", size: 3 };

  // Node 22's Response.blob() returns a Node Blob, which jsdom's FileReader rejects.
  function blobResponse(bytes: Uint8Array): Response {
    const res = new Response(bytes);
    res.blob = () => Promise.resolve(new Blob([bytes]));
    return res;
  }

  async function rejectedThenRetry(text: string, blob: () => Response) {
    calls = installAcpFakes(({ url }) => (url.includes("/acp/attachments/") ? blob() : undefined));
    const { result } = await openSession("s-retry");
    lastSocket().message({
      session_id: "s-retry",
      seq: 5,
      event: { PromptRejected: { reason: "prompt_failed", text, attachments: [image] } },
    });
    await flushAsync();
    const rejected = result.current.state.rejectedPrompts[0]!;
    await act(() => result.current.retryRejectedPrompt(rejected));
    return result;
  }

  it.each([
    ["a captioned image", "look at this"],
    ["an attachment-only prompt", ""],
  ])("resends %s with its stored attachment", async (_label, text) => {
    await rejectedThenRetry(text, () => blobResponse(new Uint8Array([112, 110, 103])));
    expect(calls.some((c) => c.method === "GET" && c.url === "/api/sessions/s-retry/acp/attachments/att-1")).toBe(true);
    const sent = posts("/acp/prompt");
    expect(sent).toHaveLength(1);
    expect(JSON.parse(sent[0]!.body!)).toMatchObject({
      text,
      attachments: [{ kind: "image", mime_type: "image/png", name: "shot.png", data: "cG5n" }],
    });
  });

  it("does not resend without an attachment it cannot reload", async () => {
    const result = await rejectedThenRetry("look at this", () => new Response("gone", { status: 404 }));
    expect(posts("/acp/prompt")).toHaveLength(0);
    expect(result.current.state.lastError).toMatch(/attachments/);
  });
});

describe("resolveApproval outcomes", () => {
  it.each([
    ["a 500", () => new Response("boom", { status: 500 }), /Could not resolve approval \(500\)/],
    [
      "a network failure",
      () => {
        throw new TypeError("network down");
      },
      /Network error resolving approval/,
    ],
  ])("rejects and reports %s so the card can retry", async (_label, respond, error) => {
    installAcpFakes(({ url }) => (url.includes("/acp/approvals/") ? respond() : undefined));
    const { result } = render("sess-approve");
    await flushAsync();
    await act(() => expect(result.current.resolveApproval("n-1", "Allow")).rejects.toThrow());
    expect(result.current.state.lastError).toMatch(error);
  });

  it.each([
    ["a 200", () => json({})],
    ["a 404 for an approval already resolved", () => new Response("no pending approval n-1", { status: 404 })],
  ])("resolves on %s", async (_label, respond) => {
    calls = installAcpFakes(({ url }) => (url.includes("/acp/approvals/") ? respond() : undefined));
    const { result } = render("sess-approve");
    await flushAsync();
    await act(() => result.current.resolveApproval("n-1", "Allow", "opt-1"));
    const [post] = posts("/api/sessions/sess-approve/acp/approvals/n-1");
    expect(JSON.parse(post!.body!)).toEqual({ decision: "Allow", option_id: "opt-1" });
    expect(result.current.state.lastError).toBeFalsy();
  });
});

describe("setConfigOption", () => {
  it("posts the config id and value", async () => {
    const { result } = render("sess-cfg");
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
    const { result } = render("sess-cfg-fail");
    await flushAsync();
    await act(() => result.current.setConfigOption("model", "claude-sonnet-4-6"));
    expect(result.current.state.pendingConfigOption).toBeNull();
    expect(result.current.state.lastError).toMatch(error);
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

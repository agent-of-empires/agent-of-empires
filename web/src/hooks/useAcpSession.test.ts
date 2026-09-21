// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { emptyAcpState } from "../lib/acpTypes";
import { STATE_TTL_MS, STORAGE_KEY_PREFIX } from "../lib/acpStateStorage";
import { reportAcpInteraction, type ServerQueuedPrompt } from "../lib/api";
import { ACP_MAX_RETRIES, ACP_WS_STALE_MS, acpRetryDelayMs } from "./acpSession/useAcpConnection";
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
    [100, 30000],
    [0, 1000],
    [-5, 1000],
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

  it("never redials a socket that is not OPEN", async () => {
    render("sess-connecting");
    await flushAsync();
    await act(() => vi.advanceTimersByTimeAsync(ACP_WS_STALE_MS + 30000));
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

    it.each([
      ["archived", ["2026-01-01T00:00:00Z", null], "/archive", { archived: false, kill_pane: true }],
      ["snoozed", [null, "2099-01-01T00:00:00Z"], "/snooze", { minutes: null }],
      [
        "archived and snoozed",
        ["2026-01-01T00:00:00Z", "2099-01-01T00:00:00Z"],
        "/archive",
        { archived: false, kill_pane: true },
      ],
    ] as const)("wakes a %s session before sending", async (_label, [archived, snoozed], suffix, body) => {
      calls = installAcpFakes(queuedPrompt);
      const { result } = render("sess-wake", "absent", archived, snoozed);
      await flushAsync();
      await act(() => result.current.sendPrompt("wake me up"));
      await flushAsync();
      expect(patches("/archive").length + patches("/snooze").length).toBe(1);
      expect(JSON.parse(patches(suffix)[0]!.body!)).toEqual(body);
      expect(result.current.state.queuedPrompts.map((q) => q.text)).toEqual(["wake me up"]);
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

    it.each([
      ["/archive", "2026-01-01T00:00:00Z", null],
      ["/snooze", null, "2099-01-01T00:00:00Z"],
    ])("sends nothing when the %s wake fails", async (suffix, archived, snoozed) => {
      calls = installAcpFakes(failingWake(suffix));
      const { result } = render("sess-wake-fail", "absent", archived, snoozed);
      await flushAsync();
      await act(() => result.current.sendPrompt("wake me up"));
      await flushAsync();
      expect(posts("/acp/prompt")).toHaveLength(0);
      expect(result.current.state.queuedPrompts).toHaveLength(0);
      expect(result.current.state.lastError).toMatch(/wake/i);
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
    const { result } = render("sess-capacity", "running", null, null);
    await flushAsync();
    await act(() => result.current.sendPrompt("no room at the inn"));
    await flushAsync();
    expect(posts("/queue")).toHaveLength(0);
    expect(result.current.state.lastError).toMatch(/capacity/i);
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

    it("removes a row server-side, then sends it", async () => {
      serverQueue.set("t1", row("t1", "text only"));
      await sendNow("t1");
      expect(deletes()).toHaveLength(1);
      expect(posts("/acp/prompt")).toHaveLength(1);
      expect(serverQueue.has("t1")).toBe(false);
    });

    it("leaves a row whose attachment bytes live only on the server", async () => {
      const attachments = [{ id: "a", kind: "image" as const, mime_type: "image/png", name: "s.png", size: 9 }];
      serverQueue.set("img", row("img", "caption", { attachments }));
      const result = await sendNow("img");
      expect([deletes(), posts("/acp/prompt")]).toEqual([[], []]);
      expect(result.current.state.queuedPrompts.map((q) => q.id)).toContain("img");
    });

    it("sends nothing when the drain claimed the row first", async () => {
      serverQueue.set("r1", row("r1", "raced"));
      await sendNow("r1", () => serverQueue.delete("r1"));
      expect(posts("/acp/prompt")).toHaveLength(0);
    });
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

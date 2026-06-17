// @vitest-environment jsdom
//
// Exercises the recent-first cold open and scroll-up `loadOlder` network
// path end-to-end in the hook (#2236): the tail fetch via `before`, the
// long-session handshake-prefix backfill, and an older page prepended on
// loadOlder. The mocked Playwright specs only cover transcripts that fit
// in one page, so these branches need the hook mounted with scripted
// replay responses.

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { clearAcpCache, useAcpSession } from "./useAcpSession";

interface FakeSocket {
  url: string;
  readyState: number;
  onopen: ((ev: Event) => void) | null;
  onclose: ((ev: CloseEvent) => void) | null;
  onerror: ((ev: Event) => void) | null;
  onmessage: ((ev: MessageEvent) => void) | null;
  close: () => void;
  send: () => void;
}

const sockets: FakeSocket[] = [];
let originalWebSocket: typeof WebSocket;

class FakeWebSocket implements FakeSocket {
  url: string;
  readyState = 0;
  onopen: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  constructor(url: string) {
    this.url = url;
    sockets.push(this);
  }
  close(): void {
    this.readyState = FakeWebSocket.CLOSED;
  }
  send(): void {
    /* no-op */
  }
}

const prompt = (seq: number, text: string) => ({ session_id: "sess-rf", seq, event: { UserPromptSent: { text } } });
const caps = (seq: number) => ({
  session_id: "sess-rf",
  seq,
  event: { PromptCapabilities: { image: true, audio: false, embedded_context: true } },
});

function replayBody(frames: unknown[], next: number | null, hasMore: boolean) {
  return JSON.stringify({
    frames,
    lost: false,
    highest_seq: 10,
    lowest_seq: 1,
    next_cursor: next,
    has_more: hasMore,
  });
}

beforeEach(() => {
  sockets.length = 0;
  // The reduced-state cache is module-level; clear it so one test's
  // session can't hydrate another's and skip the cold-open path.
  clearAcpCache();
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = typeof input === "string" ? input : input.toString();
      if (url.includes("/api/login/status")) {
        return new Response(JSON.stringify({ required: false, authenticated: true, elevated: true }), { status: 200 });
      }
      if (url.includes("/acp/replay")) {
        const q = new URL(url, "http://x").searchParams;
        if (
          q.get("before") &&
          Number(q.get("before")) <= 10 &&
          q.get("before") !== null &&
          Number(q.get("before")) < 100
        ) {
          // Older page requested via the loadOlder cursor (before=6).
          return new Response(
            replayBody([prompt(2, "p2"), prompt(3, "p3"), prompt(4, "p4"), prompt(5, "p5")], 2, false),
            {
              status: 200,
            },
          );
        }
        if (q.get("before")) {
          // Tail (before = MAX): newest page, more history remains.
          return new Response(
            replayBody(
              [prompt(6, "p6"), prompt(7, "p7"), prompt(8, "p8"), prompt(9, "p9"), prompt(10, "p10")],
              6,
              true,
            ),
            { status: 200 },
          );
        }
        // since=0 handshake prefix.
        return new Response(replayBody([caps(1)], 1, true), { status: 200 });
      }
      return new Response(JSON.stringify({ frames: [], lost: false, highest_seq: 0 }), { status: 200 });
    }),
  );
  originalWebSocket = global.WebSocket;
  global.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
});

afterEach(() => {
  global.WebSocket = originalWebSocket;
  vi.unstubAllGlobals();
});

async function flush(): Promise<void> {
  await act(async () => {
    for (let i = 0; i < 10; i += 1) await Promise.resolve();
  });
}

describe("useAcpSession recent-first cold open + loadOlder (#2236)", () => {
  it("renders the tail, backfills the handshake, then prepends older on loadOlder", async () => {
    const { result } = renderHook(() => useAcpSession("sess-rf"));
    await flush();

    // Tail rendered (prompts 6..10), more history flagged, handshake
    // capabilities projected from the seq-0 prefix even though those rows
    // are not in the transcript window.
    expect(result.current.state.activity.map((r) => r.id)).toEqual([
      "user-seq-6",
      "user-seq-7",
      "user-seq-8",
      "user-seq-9",
      "user-seq-10",
    ]);
    expect(result.current.hasMoreOlder).toBe(true);
    expect(result.current.state.promptCapabilities).toEqual({ image: true, audio: false, embeddedContext: true });
    expect(result.current.state.oldestSeq).toBe(6);

    // Scroll-up fetch: the older page prepends ahead of the tail and
    // clears hasMoreOlder once the start is reached.
    await act(async () => {
      await result.current.loadOlder();
    });
    await flush();

    expect(result.current.state.activity.map((r) => r.id)).toEqual([
      "user-seq-2",
      "user-seq-3",
      "user-seq-4",
      "user-seq-5",
      "user-seq-6",
      "user-seq-7",
      "user-seq-8",
      "user-seq-9",
      "user-seq-10",
    ]);
    expect(result.current.state.oldestSeq).toBe(2);
    expect(result.current.hasMoreOlder).toBe(false);
  });

  it("flags a lagged transcript when the tail reports lost", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = typeof input === "string" ? input : input.toString();
        if (url.includes("/api/login/status")) {
          return new Response(JSON.stringify({ required: false, authenticated: true, elevated: true }), {
            status: 200,
          });
        }
        if (url.includes("/acp/replay")) {
          return new Response(
            JSON.stringify({ frames: [], lost: true, highest_seq: 9999, lowest_seq: 5000, has_more: false }),
            { status: 200 },
          );
        }
        return new Response(JSON.stringify({ frames: [], lost: false, highest_seq: 0 }), { status: 200 });
      }),
    );
    const { result } = renderHook(() => useAcpSession("sess-lost"));
    await flush();
    expect(result.current.state.lagged).toBe(true);
    expect(result.current.hasMoreOlder).toBe(false);
  });

  it("resets older-paging state when the session changes", async () => {
    const { result, rerender } = renderHook(({ id }: { id: string }) => useAcpSession(id), {
      initialProps: { id: "sess-rf" },
    });
    await flush();
    expect(result.current.hasMoreOlder).toBe(true);

    rerender({ id: "sess-rf-2" });
    // The switch clears the flags synchronously before the new session's
    // own recent-first load runs.
    expect(result.current.loadingOlder).toBe(false);
    await flush();
    await flush();
    // The fresh session re-derives its own watermark from its tail.
    expect(result.current.hasMoreOlder).toBe(true);
  });
});

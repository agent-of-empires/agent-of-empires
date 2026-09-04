// @vitest-environment jsdom
//
// Regression tests for #3688: the redelivery-cap park is the second
// daemon-side state that answers "sent" for a session with no worker. The
// client's compensating enqueue and its "Send now" affordance were both
// keyed on the first one (`workerIdleStopped`) alone, so a recovery prompt
// that 503'd on a slow resume vanished with no message, and a queued row
// stranded behind the park had no manual send.

import { act, renderHook } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { AgentProfileProvider } from "../lib/agentProfileContext";
import { useAcpSession } from "./useAcpSession";

const sockets: FakeWebSocket[] = [];

class FakeWebSocket {
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

async function flushAsync(): Promise<void> {
  await act(async () => {
    for (let i = 0; i < 8; i++) {
      await Promise.resolve();
    }
  });
}

const wrapper = ({ children }: { children: ReactNode }) =>
  createElement(AgentProfileProvider, { toolKey: "claude" }, children);

describe("useAcpSession rate-limit redelivery-cap park (#3688)", () => {
  let originalWebSocket: typeof WebSocket;
  let queuePosts: number;

  function stubFetch(parked: boolean) {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === "string" ? input : input.toString();
        const method = init?.method ?? "GET";
        if (url.includes("/acp/replay")) {
          const frames = parked
            ? [{ session_id: "sess-cap", seq: 1, event: { Stopped: { reason: "rate_limit_exhausted_retries" } } }]
            : [];
          return new Response(JSON.stringify({ frames, lost: false, highest_seq: parked ? 1 : 0 }), { status: 200 });
        }
        // The park routes the prompt through `send_turn`, whose resume did
        // not produce a live worker inside the readiness window.
        if (url.includes("/acp/prompt") && method === "POST") {
          return new Response("worker_not_ready", { status: 503 });
        }
        if (url.includes("/queue")) {
          if (method === "GET") return new Response("[]", { status: 200 });
          if (method === "POST") {
            queuePosts += 1;
            const parsed = JSON.parse(String(init?.body)) as { id: string; text: string };
            return new Response(
              JSON.stringify({ id: parsed.id, seq: 1, text: parsed.text, created_at: "2026-01-01T00:00:00Z" }),
              { status: 200 },
            );
          }
        }
        return new Response("{}", { status: 200 });
      }),
    );
  }

  async function openSocket(): Promise<void> {
    await act(async () => {
      for (const s of sockets) {
        s.readyState = FakeWebSocket.OPEN;
        s.onopen?.(new Event("open"));
      }
    });
  }

  beforeEach(() => {
    sockets.length = 0;
    queuePosts = 0;
    originalWebSocket = global.WebSocket;
    global.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
  });

  afterEach(() => {
    global.WebSocket = originalWebSocket;
    vi.unstubAllGlobals();
  });

  it("re-enqueues a recovery prompt the daemon accepted then 503'd", async () => {
    stubFetch(true);
    const { result } = renderHook(() => useAcpSession("sess-cap", "running", null, null), { wrapper });
    await flushAsync();
    expect(result.current.state.rateLimitRetriesExhausted).toBe(true);

    await act(async () => {
      await result.current.sendPrompt("try again after the cap");
    });
    await flushAsync();

    expect(queuePosts).toBe(1);
    expect(result.current.state.queuedPrompts).toHaveLength(1);
    expect(result.current.state.queuedPrompts[0]!.text).toBe("try again after the cap");
  });

  it("drops it when the session is in neither workerless-but-sendable state", async () => {
    // The control: without a park (and without idle dormancy) a 503 means the
    // daemon queued nothing and the client must not invent a row.
    stubFetch(false);
    const { result } = renderHook(() => useAcpSession("sess-cap-live", "running", null, null), { wrapper });
    await flushAsync();
    expect(result.current.state.rateLimitRetriesExhausted).toBe(false);

    await act(async () => {
      await result.current.sendPrompt("ordinary prompt");
    });
    await flushAsync();

    expect(queuePosts).toBe(0);
  });

  it("offers Send now on a row stranded behind the park", async () => {
    stubFetch(true);
    const { result } = renderHook(() => useAcpSession("sess-cap-send", "absent", null, null), { wrapper });
    await flushAsync();
    await openSocket();
    await flushAsync();

    expect(result.current.status).toBe("open");
    expect(result.current.state.rateLimitRetriesExhausted).toBe(true);
    expect(result.current.canSendQueuedNow).toBe(true);
  });
});

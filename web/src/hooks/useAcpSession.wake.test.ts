// @vitest-environment jsdom
//
// Regression tests for #1581: sending a prompt to an archived or
// snoozed structured view session must auto-clear that flag client-side
// before enqueueing locally. Without the wake, the structured view
// reconciler keeps skipping the session (its respawn predicate
// excludes archived + actively-snoozed rows), the worker stays
// down, and the queued prompt never drains.

import { act, renderHook } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { AgentProfileProvider } from "../lib/agentProfileContext";
import { useAcpSession } from "./useAcpSession";

interface FakeSocket {
  url: string;
  readyState: number;
  onopen: ((ev: Event) => void) | null;
  onclose: ((ev: CloseEvent) => void) | null;
  onerror: ((ev: Event) => void) | null;
  onmessage: ((ev: MessageEvent) => void) | null;
  close: () => void;
  send: (data: string | ArrayBufferLike | Blob | ArrayBufferView) => void;
}

const sockets: FakeSocket[] = [];
let originalWebSocket: typeof WebSocket;

class FakeWebSocket implements FakeSocket {
  url: string;
  readyState: number = 0;
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

describe("useAcpSession auto-wake on sendPrompt (#1581)", () => {
  interface Call {
    url: string;
    method: string;
    body: unknown;
  }
  let calls: Call[];

  beforeEach(() => {
    sockets.length = 0;
    calls = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === "string" ? input : input.toString();
        const method = init?.method ?? "GET";
        let body: unknown = null;
        if (typeof init?.body === "string") {
          try {
            body = JSON.parse(init.body);
          } catch {
            body = init.body;
          }
        }
        calls.push({ url, method, body });
        if (url.includes("/acp/replay")) {
          return new Response(JSON.stringify({ frames: [], lost: false, highest_seq: 0 }), { status: 200 });
        }
        if (url.endsWith("/archive") && method === "PATCH") {
          return new Response(JSON.stringify({ id: "sess-wake-PLACEHOLDER", archived_at: null }), { status: 200 });
        }
        if (url.endsWith("/snooze") && method === "PATCH") {
          return new Response(
            JSON.stringify({
              id: "sess-wake-PLACEHOLDER",
              snoozed_until: null,
            }),
            { status: 200 },
          );
        }
        return new Response("{}", { status: 200 });
      }),
    );
    originalWebSocket = global.WebSocket;
    global.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
  });

  afterEach(() => {
    global.WebSocket = originalWebSocket;
    vi.unstubAllGlobals();
  });

  it("clears the archived flag via PATCH before enqueueing the prompt", async () => {
    const sessionId = "sess-wake-archive";
    const { result } = renderHook(() => useAcpSession(sessionId, "absent", "2026-01-01T00:00:00Z", null), { wrapper });
    await flushAsync();

    await act(async () => {
      await result.current.sendPrompt("wake me up");
    });
    await flushAsync();

    const archiveCalls = calls.filter((c) => c.url.endsWith("/archive") && c.method === "PATCH");
    expect(archiveCalls).toHaveLength(1);
    expect(archiveCalls[0]!.body).toEqual({
      archived: false,
      kill_pane: true,
    });
    // Worker is "absent", so the prompt enqueues rather than POSTs.
    expect(result.current.state.queuedPrompts).toHaveLength(1);
    expect(result.current.state.queuedPrompts[0]!.text).toBe("wake me up");
  });

  it("clears the snoozed flag via PATCH before enqueueing the prompt", async () => {
    const sessionId = "sess-wake-snooze";
    const { result } = renderHook(() => useAcpSession(sessionId, "absent", null, "2099-01-01T00:00:00Z"), { wrapper });
    await flushAsync();

    await act(async () => {
      await result.current.sendPrompt("wake me up");
    });
    await flushAsync();

    const snoozeCalls = calls.filter((c) => c.url.endsWith("/snooze") && c.method === "PATCH");
    expect(snoozeCalls).toHaveLength(1);
    expect(snoozeCalls[0]!.body).toEqual({ minutes: null });
    expect(result.current.state.queuedPrompts).toHaveLength(1);
  });

  it("does not call wake endpoints when the session is live", async () => {
    const sessionId = "sess-wake-live";
    const { result } = renderHook(() => useAcpSession(sessionId, "absent", null, null), { wrapper });
    await flushAsync();

    await act(async () => {
      await result.current.sendPrompt("just a prompt");
    });
    await flushAsync();

    const wakeCalls = calls.filter(
      (c) => c.method === "PATCH" && (c.url.endsWith("/archive") || c.url.endsWith("/snooze")),
    );
    expect(wakeCalls).toHaveLength(0);
    expect(result.current.state.queuedPrompts).toHaveLength(1);
  });

  it("does NOT enqueue when the archive wake call fails", async () => {
    // Regression: a failed wake PATCH used to fall through to the
    // local enqueue, which left the prompt parked in a queue that
    // never drains (the reconciler kept skipping the still-archived
    // session). Surface an error instead so the user knows to retry
    // or unarchive manually. See #1581 CodeRabbit review.
    const sessionId = "sess-wake-fail-arch";
    // Override the default fetch to fail the archive PATCH.
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === "string" ? input : input.toString();
        const method = init?.method ?? "GET";
        let body: unknown = null;
        if (typeof init?.body === "string") {
          try {
            body = JSON.parse(init.body);
          } catch {
            body = init.body;
          }
        }
        calls.push({ url, method, body });
        if (url.includes("/acp/replay")) {
          return new Response(JSON.stringify({ frames: [], lost: false, highest_seq: 0 }), { status: 200 });
        }
        if (url.endsWith("/archive") && method === "PATCH") {
          return new Response("simulated failure", { status: 500 });
        }
        return new Response("{}", { status: 200 });
      }),
    );

    const { result } = renderHook(() => useAcpSession(sessionId, "absent", "2026-01-01T00:00:00Z", null), { wrapper });
    await flushAsync();

    await act(async () => {
      await result.current.sendPrompt("wake me up");
    });
    await flushAsync();

    expect(result.current.state.queuedPrompts).toHaveLength(0);
    expect(result.current.state.lastError).toMatch(/wake/i);
  });

  it("does NOT enqueue when the snooze wake call fails", async () => {
    const sessionId = "sess-wake-fail-snooze";
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === "string" ? input : input.toString();
        const method = init?.method ?? "GET";
        let body: unknown = null;
        if (typeof init?.body === "string") {
          try {
            body = JSON.parse(init.body);
          } catch {
            body = init.body;
          }
        }
        calls.push({ url, method, body });
        if (url.includes("/acp/replay")) {
          return new Response(JSON.stringify({ frames: [], lost: false, highest_seq: 0 }), { status: 200 });
        }
        if (url.endsWith("/snooze") && method === "PATCH") {
          return new Response("simulated failure", { status: 500 });
        }
        return new Response("{}", { status: 200 });
      }),
    );

    const { result } = renderHook(() => useAcpSession(sessionId, "absent", null, "2099-01-01T00:00:00Z"), { wrapper });
    await flushAsync();

    await act(async () => {
      await result.current.sendPrompt("wake me up");
    });
    await flushAsync();

    expect(result.current.state.queuedPrompts).toHaveLength(0);
    expect(result.current.state.lastError).toMatch(/wake/i);
  });

  it("prefers archive over snooze when both are somehow set (defensive)", async () => {
    // The server's XOR rules prevent both flags from co-existing on
    // the same session, but a defensive client should still do
    // exactly one wake call rather than two. Archive wins because
    // it is the stronger signal (no auto-wake) and clearing it
    // implies snooze should also clear via touch_last_accessed in
    // merge_user_action_diff on the server side.
    const sessionId = "sess-wake-both";
    const { result } = renderHook(
      () => useAcpSession(sessionId, "absent", "2026-01-01T00:00:00Z", "2099-01-01T00:00:00Z"),
      { wrapper },
    );
    await flushAsync();

    await act(async () => {
      await result.current.sendPrompt("wake me up");
    });
    await flushAsync();

    const archiveCalls = calls.filter((c) => c.url.endsWith("/archive") && c.method === "PATCH");
    const snoozeCalls = calls.filter((c) => c.url.endsWith("/snooze") && c.method === "PATCH");
    expect(archiveCalls).toHaveLength(1);
    expect(snoozeCalls).toHaveLength(0);
  });
});

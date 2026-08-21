// @vitest-environment jsdom
//
// Regression tests for the persistent structured-view stack: an inactive session
// must stop its reconnect backoff and not schedule new retries, but should
// reconnect when it becomes active again.

import { act, renderHook } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { AgentProfileProvider } from "../../lib/agentProfileContext";
import { useAcpSession } from "../useAcpSession";

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

function advanceTimers(ms: number): void {
  act(() => {
    vi.advanceTimersByTime(ms);
  });
}

const wrapper = ({ children }: { children: ReactNode }) =>
  createElement(AgentProfileProvider, { toolKey: "claude" }, children);

describe("useAcpSession deactivation reconnect handling", () => {
  beforeEach(() => {
    sockets.length = 0;
    vi.useFakeTimers({ shouldAdvanceTime: false });
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        return new Response(JSON.stringify({ frames: [], lost: false, highest_seq: 0 }), { status: 200 });
      }),
    );
    originalWebSocket = global.WebSocket;
    global.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
  });

  afterEach(() => {
    global.WebSocket = originalWebSocket;
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("does not schedule a retry while the session is inactive", async () => {
    const sessionId = "sess-inactive-retry";
    const { rerender } = renderHook(({ active }) => useAcpSession(sessionId, active), {
      wrapper,
      initialProps: { active: true },
    });
    await flushAsync();

    expect(sockets).toHaveLength(1);
    const ws = sockets[0]!;

    // Flip to inactive before the close, then simulate a disconnect.
    rerender({ active: false });
    await flushAsync();

    act(() => {
      ws.readyState = FakeWebSocket.CLOSED;
      ws.onclose?.(new CloseEvent("close"));
    });
    await flushAsync();

    // Advance well past the first retry delay; no new socket should be created.
    advanceTimers(5000);
    expect(sockets).toHaveLength(1);
  });

  it("triggers a reconnect when the session becomes active again after a disconnect", async () => {
    const sessionId = "sess-active-again";
    const { rerender } = renderHook(({ active }) => useAcpSession(sessionId, active), {
      wrapper,
      initialProps: { active: false },
    });
    await flushAsync();

    // The initial mount connects even while inactive; close it and confirm no
    // retry is scheduled while still inactive.
    expect(sockets).toHaveLength(1);
    const ws = sockets[0]!;
    act(() => {
      ws.readyState = FakeWebSocket.CLOSED;
      ws.onclose?.(new CloseEvent("close"));
    });
    await flushAsync();
    advanceTimers(5000);
    expect(sockets).toHaveLength(1);

    // Becoming active should reconnect immediately.
    rerender({ active: true });
    await flushAsync();
    expect(sockets).toHaveLength(2);
  });
});

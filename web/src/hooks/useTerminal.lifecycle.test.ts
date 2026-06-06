// @vitest-environment jsdom
//
// Lifecycle tests for the useTerminal hook. Drives the connect path,
// ws.onopen/onmessage/onclose, retry backoff, and disposal against a
// FakeWebSocket + a hand-rolled Terminal mock that mimics enough of
// xterm.js's surface for the hook to mount. Companion to the pure-
// helper suites (useTerminal.theme.test.ts, useTerminal.backoff.test.ts);
// covers the WS-driven branches that those tests can't reach.

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// ── xterm.js mocks ───────────────────────────────────────────────────
// The hook builds a Terminal, loads addons (FitAddon, WebLinks,
// optionally WebGL), and calls open / onResize / onData / dispose /
// focus / write / options / attachCustomWheelEventHandler. Each mock
// is a no-op or a captured callback so the test can poke onResize
// from outside and observe what the hook sent over the WS.
//
// vi.mock factories are hoisted above any module-scope code, so the
// captured store and the fake classes live inside vi.hoisted to keep
// the factory closures self-contained.

const { captured } = vi.hoisted(() => ({
  captured: {
    disposed: false as boolean,
    writes: [] as Array<string | Uint8Array>,
    options: { fontSize: 14, theme: undefined as unknown },
    onResize: undefined as
      | ((s: { cols: number; rows: number }) => void)
      | undefined,
    onData: undefined as ((data: string) => void) | undefined,
    customWheel: undefined as ((e: WheelEvent) => boolean) | undefined,
    resizeObserverCallback: undefined as ResizeObserverCallback | undefined,
    proposedDimensions: { cols: 100, rows: 30 } as
      | { cols: number; rows: number }
      | undefined,
    oscHandlers: {} as Record<number, (data: string) => boolean>,
  },
}));

vi.mock("@xterm/xterm", () => {
  class FakeTerminal {
    cols = 80;
    rows = 24;
    options: typeof captured.options;
    element: HTMLDivElement | null = null;
    textarea: HTMLTextAreaElement | null = null;
    constructor(opts: { fontSize?: number; theme?: unknown }) {
      captured.options = { fontSize: opts.fontSize ?? 14, theme: opts.theme };
      this.options = captured.options;
    }
    loadAddon(): void {}
    open(parent: HTMLElement): void {
      this.element = document.createElement("div");
      this.element.classList.add("xterm");
      this.textarea = document.createElement("textarea");
      this.textarea.classList.add("xterm-helper-textarea");
      this.element.appendChild(this.textarea);
      parent.appendChild(this.element);
    }
    focus(): void {}
    dispose(): void {
      captured.disposed = true;
    }
    write(data: string | Uint8Array): void {
      captured.writes.push(data);
    }
    onResize(cb: (s: { cols: number; rows: number }) => void): {
      dispose: () => void;
    } {
      captured.onResize = cb;
      return { dispose: () => {} };
    }
    onData(cb: (data: string) => void): { dispose: () => void } {
      captured.onData = cb;
      return { dispose: () => {} };
    }
    attachCustomWheelEventHandler(fn: (e: WheelEvent) => boolean): void {
      captured.customWheel = fn;
    }
    attachCustomKeyEventHandler(_fn: (e: KeyboardEvent) => boolean): void {}
    parser = {
      registerOscHandler(
        id: number,
        cb: (data: string) => boolean,
      ): { dispose: () => void } {
        captured.oscHandlers[id] = cb;
        return { dispose: () => {} };
      },
    };
    resize(cols: number, rows: number): void {
      this.cols = cols;
      this.rows = rows;
      captured.onResize?.({ cols, rows });
    }
  }
  return { Terminal: FakeTerminal };
});

vi.mock("@xterm/addon-fit", () => {
  class FakeFitAddon {
    fit(): void {
      captured.onResize?.({ cols: 100, rows: 30 });
    }
    proposeDimensions(): { cols: number; rows: number } | undefined {
      return captured.proposedDimensions;
    }
  }
  return { FitAddon: FakeFitAddon };
});

vi.mock("@xterm/addon-webgl", () => {
  class FakeWebglAddon {
    onContextLoss(): void {}
    dispose(): void {}
  }
  return { WebglAddon: FakeWebglAddon };
});

vi.mock("@xterm/addon-web-links", () => {
  class FakeWebLinksAddon {}
  return { WebLinksAddon: FakeWebLinksAddon };
});

// ── ResizeObserver shim ──────────────────────────────────────────────
// jsdom doesn't ship ResizeObserver; the hook registers one on the
// terminal element. We capture the callback so the test can drive
// "container resized" events deterministically. Without this stub the
// hook constructor throws and the whole test file fails to load.

class FakeResizeObserver {
  constructor(cb: ResizeObserverCallback) {
    captured.resizeObserverCallback = cb;
  }
  observe(): void {}
  disconnect(): void {}
  unobserve(): void {}
}

// ── WebSocket fake ───────────────────────────────────────────────────

interface FakeSocket {
  url: string;
  protocols: string[] | string | undefined;
  readyState: number;
  onopen: ((ev: Event) => void) | null;
  onclose: ((ev: CloseEvent) => void) | null;
  onerror: ((ev: Event) => void) | null;
  onmessage: ((ev: MessageEvent) => void) | null;
  binaryType: string;
  sent: Array<string | Uint8Array>;
  close: () => void;
  send: (data: string | ArrayBufferLike | Blob | ArrayBufferView) => void;
}

const sockets: FakeSocket[] = [];
let originalWebSocket: typeof WebSocket;
let originalResizeObserver: typeof ResizeObserver | undefined;

class FakeWebSocket implements FakeSocket {
  url: string;
  protocols: string[] | string | undefined;
  readyState = 0;
  onopen: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  binaryType = "blob";
  sent: Array<string | Uint8Array> = [];
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  constructor(url: string, protocols?: string | string[]) {
    this.url = url;
    this.protocols = protocols;
    sockets.push(this);
  }
  close(): void {
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.({
      code: 1006,
      reason: "test close",
      wasClean: false,
    } as CloseEvent);
  }
  send(data: string | ArrayBufferLike | Blob | ArrayBufferView): void {
    if (typeof data === "string") this.sent.push(data);
    else this.sent.push(new Uint8Array(data as ArrayBuffer));
  }
}

beforeEach(() => {
  vi.useFakeTimers();
  sockets.length = 0;
  captured.disposed = false;
  captured.writes = [];
  captured.onResize = undefined;
  captured.onData = undefined;
  captured.customWheel = undefined;
  captured.resizeObserverCallback = undefined;
  captured.proposedDimensions = { cols: 100, rows: 30 };
  captured.oscHandlers = {};
  originalWebSocket = global.WebSocket;
  global.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
  originalResizeObserver = global.ResizeObserver;
  global.ResizeObserver =
    FakeResizeObserver as unknown as typeof ResizeObserver;
  // localStorage starts empty so the hook reads bundled-default font
  // sizes and theme colors.
  window.localStorage.clear();
});

afterEach(() => {
  global.WebSocket = originalWebSocket;
  if (originalResizeObserver) global.ResizeObserver = originalResizeObserver;
  else
    (global as unknown as { ResizeObserver: undefined }).ResizeObserver =
      undefined;
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

async function flushAsync(times = 8): Promise<void> {
  await act(async () => {
    for (let i = 0; i < times; i++) await Promise.resolve();
  });
}

function fireResizeObserver(width: number, height: number): void {
  const entry = {
    contentRect: { width, height },
  } as ResizeObserverEntry;
  captured.resizeObserverCallback?.([entry], {} as ResizeObserver);
}

// renderHook attaches its container to document.body for us; the
// tests below just need to wire each session's div into the hook's
// containerRef so the connect effect sees a mounted element.
import { useTerminal } from "./useTerminal";

describe("useTerminal lifecycle", () => {
  it("does not open a WebSocket while sessionId is null", async () => {
    renderHook(() => useTerminal(null, "ws", false, false));
    await flushAsync();
    expect(sockets).toHaveLength(0);
  });

  it("opens a WebSocket once a sessionId becomes available", async () => {
    // Mount with a div so containerRef has somewhere to attach.
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result, rerender } = renderHook(
        (props: { id: string | null }) => {
          const term = useTerminal(props.id, "ws", false, false);
          if (term.containerRef && !term.containerRef.current) {
            (
              term.containerRef as unknown as {
                current: HTMLDivElement | null;
              }
            ).current = div;
          }
          return term;
        },
        { initialProps: { id: null } },
      );
      expect(sockets).toHaveLength(0);
      rerender({ id: "s-1" });
      await flushAsync();
      expect(sockets).toHaveLength(1);
      expect(sockets[0]!.url).toContain("/sessions/s-1/ws");
      expect(result.current.state.connected).toBe(false);
    } finally {
      div.remove();
    }
  });

  it("ws.onopen flips state.connected and sends activate + resize", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-2", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      expect(sockets).toHaveLength(1);
      const ws = sockets[0]!;

      // Drive ws.onopen and let the rAF + debounce settle so the
      // initial resize lands on the server.
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();
      await act(async () => {
        await vi.advanceTimersByTimeAsync(300);
      });
      await flushAsync();

      expect(result.current.state.connected).toBe(true);
      expect(result.current.state.isPrimary).toBe(true);

      // Activate JSON message must have been queued on connect.
      const activate = ws.sent.find(
        (m) => typeof m === "string" && m.includes('"activate"'),
      );
      expect(activate).toBeDefined();
      // FitAddon's stub emits cols=100/rows=30, so the resize message
      // should reflect that exact pair.
      const resize = ws.sent.find(
        (m) =>
          typeof m === "string" && m.includes('"resize"') && m.includes("100"),
      );
      expect(resize).toBeDefined();
    } finally {
      div.remove();
    }
  });

  it("claimPrimary=false keeps the socket warm without auto-activating", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      renderHook(() => {
        const term = useTerminal("s-passive", "ws", false, false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;

      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();

      const activate = ws.sent.find(
        (m) => typeof m === "string" && m.includes('"activate"'),
      );
      expect(activate).toBeUndefined();
    } finally {
      div.remove();
    }
  });

  it("suppresses tiny resize messages from hidden containers", async () => {
    // Hidden container = no offsetParent + a tiny proposed grid. The
    // hook should never let that bogus measurement reach the server.
    // Simulate by stubbing the FakeFitAddon's fit() to emit (10, 4)
    // before any RO fires, and force offsetParent to null so the
    // hidden-container guard engages even though jsdom's default
    // would normally let any size through.
    const FakeFitAddonClass = (await import("@xterm/addon-fit"))
      .FitAddon as unknown as {
      prototype: { fit: () => void };
    };
    const origFit = FakeFitAddonClass.prototype.fit;
    FakeFitAddonClass.prototype.fit = function () {
      captured.onResize?.({ cols: 10, rows: 4 });
    };
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-hidden", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();
      await act(async () => {
        await vi.advanceTimersByTimeAsync(400);
      });
      await flushAsync();

      // Activate is fine (it does not carry a measurement), but no
      // resize message should have shipped at the tiny grid.
      const tinyResize = ws.sent.find(
        (m) =>
          typeof m === "string" &&
          m.includes('"resize"') &&
          m.includes('"cols":10') &&
          m.includes('"rows":4'),
      );
      expect(tinyResize).toBeUndefined();
      expect(result.current.state.connected).toBe(true);
    } finally {
      FakeFitAddonClass.prototype.fit = origFit;
      div.remove();
    }
  });

  it("does not refit when live output triggers ResizeObserver with unchanged bounds", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      renderHook(() => {
        const term = useTerminal("s-live-output", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();

      // Prime the observer with the real container bounds. Browsers fire
      // an initial ResizeObserver notification after observe(), and the
      // hook should remember that content-box size.
      act(() => {
        fireResizeObserver(800, 500);
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(300);
      });
      await flushAsync();

      const before = ws.sent.length;
      captured.proposedDimensions = { cols: 101, rows: 31 };
      act(() => {
        ws.onmessage?.({
          data: new TextEncoder().encode("cursor output").buffer,
        } as MessageEvent);
        fireResizeObserver(800, 500);
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(100);
      });
      await flushAsync();

      const resizeAfterOutput = ws.sent
        .slice(before)
        .find((m) => typeof m === "string" && m.includes('"resize"'));
      expect(resizeAfterOutput).toBeUndefined();

      act(() => {
        fireResizeObserver(900, 500);
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(100);
      });
      await flushAsync();
      const resizeAfterRealChange = ws.sent
        .slice(before)
        .find(
          (m) =>
            typeof m === "string" &&
            m.includes('"resize"') &&
            m.includes('"cols":101') &&
            m.includes('"rows":31'),
        );
      expect(resizeAfterRealChange).toBeDefined();
    } finally {
      div.remove();
    }
  });

  it("clears retryCount once the first ws.onmessage arrives", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-3", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();
      // Now drop and reconnect once so retryCount > 0 before the
      // first message lands. Need to force the retry path which only
      // fires when readyState is CLOSED at close-time.
      act(() => {
        ws.readyState = FakeWebSocket.CLOSED;
        ws.onclose?.({
          code: 1006,
          reason: "",
          wasClean: false,
        } as CloseEvent);
      });
      expect(result.current.state.retryCount).toBe(1);

      // Advance past the first backoff so a fresh socket opens.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(1500);
      });
      await flushAsync();
      expect(sockets.length).toBeGreaterThan(1);
      const ws2 = sockets[sockets.length - 1]!;
      act(() => {
        ws2.readyState = FakeWebSocket.OPEN;
        ws2.onopen?.(new Event("open"));
        ws2.onmessage?.({
          data: new TextEncoder().encode("hi").buffer,
        } as MessageEvent);
      });
      await flushAsync();
      // The first payload byte resets the counter.
      expect(result.current.state.retryCount).toBe(0);
      // And the terminal received the bytes via term.write.
      expect(captured.writes.length).toBeGreaterThan(0);
    } finally {
      div.remove();
    }
  });

  it("primary_status JSON control message updates state.isPrimary", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-4", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
        ws.onmessage?.({
          data: JSON.stringify({
            type: "primary_status",
            is_primary: false,
          }),
        } as MessageEvent);
      });
      await flushAsync();
      expect(result.current.state.isPrimary).toBe(false);
    } finally {
      div.remove();
    }
  });

  it("server close code 4001 short-circuits to retries exhausted", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-5", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.CLOSED;
        ws.onclose?.({
          code: 4001,
          reason: "pty dead",
          wasClean: false,
        } as CloseEvent);
      });
      await flushAsync();
      // No backoff was armed, the retry counter jumped past max.
      expect(result.current.state.reconnecting).toBe(false);
      expect(result.current.state.retryCount).toBe(result.current.maxRetries);
    } finally {
      div.remove();
    }
  });

  it("manualReconnect dials a fresh socket when the previous one is already CLOSED", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-6", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.CLOSED;
      });
      // Without dialing a fresh ws, calling close() would be a no-op
      // and onclose wouldn't fire. manualReconnect must detect this
      // and open a new socket directly.
      act(() => {
        result.current.manualReconnect();
      });
      await flushAsync();
      expect(sockets.length).toBeGreaterThanOrEqual(2);
    } finally {
      div.remove();
    }
  });

  it("disposes the Terminal and closes the WS on sessionId change", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { rerender } = renderHook(
        (props: { id: string | null }) => {
          const term = useTerminal(props.id, "ws", false, false);
          if (term.containerRef && !term.containerRef.current) {
            (
              term.containerRef as unknown as {
                current: HTMLDivElement | null;
              }
            ).current = div;
          }
          return term;
        },
        { initialProps: { id: "s-7" } },
      );
      await flushAsync();
      expect(sockets).toHaveLength(1);
      expect(captured.disposed).toBe(false);

      rerender({ id: "s-8" });
      await flushAsync();
      // First session's terminal was disposed; a new socket opened
      // for the next session.
      expect(captured.disposed).toBe(true);
      expect(sockets.length).toBeGreaterThanOrEqual(2);
    } finally {
      div.remove();
    }
  });

  it("session change nulls the old socket's handlers before close (no ghost retry)", async () => {
    // Regression for #1455: a session change must detach the previous
    // socket's onopen/onmessage/onclose/onerror BEFORE close(), otherwise
    // the still-bound onclose closure runs after cleanup and schedules a
    // setTimeout that calls connect() on the OLD sessionId, dialing a
    // ghost socket and overwriting wsRef.current for the new session.
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { rerender } = renderHook(
        (props: { id: string | null }) => {
          const term = useTerminal(props.id, "ws", false, false);
          if (term.containerRef && !term.containerRef.current) {
            (
              term.containerRef as unknown as {
                current: HTMLDivElement | null;
              }
            ).current = div;
          }
          return term;
        },
        { initialProps: { id: "s-old" } },
      );
      await flushAsync();
      const oldWs = sockets[0]!;
      expect(oldWs.url).toContain("/sessions/s-old/ws");
      expect(oldWs.onclose).not.toBeNull();

      // Switch sessions. Cleanup should detach handlers before close().
      rerender({ id: "s-new" });
      await flushAsync();

      // All four lifecycle handlers must be cleared on the orphaned socket.
      expect(oldWs.onopen).toBeNull();
      expect(oldWs.onmessage).toBeNull();
      expect(oldWs.onclose).toBeNull();
      expect(oldWs.onerror).toBeNull();

      // Even if the browser had a queued close event for the old socket,
      // with onclose nulled it cannot schedule a setTimeout retry. Advance
      // past every retry delay and confirm no third (ghost) socket was
      // dialed for the OLD sessionId.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(12_000);
      });
      await flushAsync();
      const ghostForOldSession = sockets
        .slice(1)
        .find((s) => s.url.includes("/sessions/s-old/ws"));
      expect(ghostForOldSession).toBeUndefined();
    } finally {
      div.remove();
    }
  });

  it("server close code 1011 falls through to the standard retry path", async () => {
    // Regression for #1455: the server now sends 1011 <reason> close
    // frames on openpty/spawn/clone-reader/take-writer failures instead
    // of opaque 1006. The client must treat 1011 as a regular retryable
    // failure (not the 4001 short-circuit).
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-1011", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.CLOSED;
        ws.onclose?.({
          code: 1011,
          reason: "openpty_failed",
          wasClean: false,
        } as CloseEvent);
      });
      await flushAsync();
      expect(result.current.state.reconnecting).toBe(true);
      expect(result.current.state.retryCount).toBe(1);
      expect(result.current.state.retryCount).toBeLessThan(
        result.current.maxRetries,
      );
    } finally {
      div.remove();
    }
  });

  it("server close code 1013 (tmux_not_ready) falls through to the standard retry path", async () => {
    // Regression for #1455: server's pane-readiness poll closes with
    // 1013 tmux_not_ready when the bounded wait elapses. Client must
    // retry on the fast-start ladder, NOT short-circuit to exhausted.
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-1013", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.CLOSED;
        ws.onclose?.({
          code: 1013,
          reason: "tmux_not_ready",
          wasClean: false,
        } as CloseEvent);
      });
      await flushAsync();
      expect(result.current.state.reconnecting).toBe(true);
      expect(result.current.state.retryCount).toBe(1);
      // The next retry must fire on the fast-start ladder, not on the
      // old 1s+ exponential delay.
      const before = sockets.length;
      await act(async () => {
        await vi.advanceTimersByTimeAsync(250);
      });
      await flushAsync();
      expect(sockets.length).toBeGreaterThan(before);
    } finally {
      div.remove();
    }
  });

  it("Ctrl+wheel zooms the font size and persists it after the debounce", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      renderHook(() => {
        const term = useTerminal("s-zoom", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      expect(captured.customWheel).toBeDefined();

      const baseline = captured.options.fontSize;
      // Drive several Ctrl+wheel-up events; each one should bump the
      // font size and the custom handler must return false (so xterm's
      // built-in zoom doesn't double-apply).
      for (let i = 0; i < 30; i++) {
        const e = new WheelEvent("wheel", {
          deltaY: -100,
          ctrlKey: true,
          bubbles: true,
          cancelable: true,
        });
        const result = captured.customWheel!(e);
        expect(result).toBe(false);
      }
      // Advance past the WHEEL_PERSIST_DEBOUNCE_MS = 400ms gate.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(500);
      });
      expect(captured.options.fontSize).toBeGreaterThan(baseline);
    } finally {
      div.remove();
    }
  });

  it("plain wheel events emit SGR mouse-wheel sequences over the WS", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      renderHook(() => {
        const term = useTerminal("s-scroll", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();
      const before = ws.sent.length;
      // Plain wheel-up: should emit `\x1b[<64;1;1M` SGR wheel-up
      // sequences. The customWheel returns false so xterm doesn't
      // double-handle.
      for (let i = 0; i < 5; i++) {
        const e = new WheelEvent("wheel", {
          deltaY: -120,
          bubbles: true,
          cancelable: true,
        });
        captured.customWheel!(e);
      }
      // Inspect the WS sends for the SGR sequence (sent as Uint8Array).
      const newSends = ws.sent.slice(before);
      const wheelUpSent = newSends.some((m) => {
        if (typeof m === "string") return false;
        const bytes = m;
        const ascii = Array.from(bytes)
          .map((b) => String.fromCharCode(b))
          .join("");
        return ascii.includes("[<64");
      });
      expect(wheelUpSent).toBe(true);
    } finally {
      div.remove();
    }
  });

  it("re-reads --term-* CSS vars when aoe:theme-changed fires", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    document.documentElement.style.setProperty("--term-bg", "#abc123");
    try {
      renderHook(() => {
        const term = useTerminal("s-theme", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      // Swap the CSS var, then fire the bus event.
      document.documentElement.style.setProperty("--term-bg", "#deadbe");
      act(() => {
        window.dispatchEvent(new Event("aoe:theme-changed"));
      });
      await flushAsync();
      // The terminal's options.theme should now have the swapped bg.
      const themeAny = captured.options.theme as
        | { background?: string }
        | undefined;
      expect(themeAny?.background).toBe("#deadbe");
    } finally {
      document.documentElement.style.removeProperty("--term-bg");
      div.remove();
    }
  });

  it("activate() sends an activate JSON message on focus", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-activate", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();
      const before = ws.sent.length;
      act(() => {
        result.current.activate();
      });
      const newActivate = ws.sent
        .slice(before)
        .find((m) => typeof m === "string" && m.includes('"activate"'));
      expect(newActivate).toBeDefined();
    } finally {
      div.remove();
    }
  });

  it("sendData routes a string payload to the WS as bytes", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-send", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();
      const before = ws.sent.length;
      act(() => {
        result.current.sendData("hello");
      });
      const newBytes = ws.sent.slice(before).find((m) => {
        if (typeof m === "string") return false;
        const ascii = Array.from(m)
          .map((b) => String.fromCharCode(b))
          .join("");
        return ascii === "hello";
      });
      expect(newBytes).toBeDefined();
    } finally {
      div.remove();
    }
  });

  it("window 'focus' sends an activate message", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      renderHook(() => {
        const term = useTerminal("s-focus", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();
      const before = ws.sent.length;
      act(() => {
        window.dispatchEvent(new Event("focus"));
      });
      const sentActivate = ws.sent
        .slice(before)
        .find((m) => typeof m === "string" && m.includes('"activate"'));
      expect(sentActivate).toBeDefined();
    } finally {
      div.remove();
    }
  });

  it("document visibilitychange to visible sends activate", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      renderHook(() => {
        const term = useTerminal("s-vis", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();
      const before = ws.sent.length;
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "visible",
      });
      act(() => {
        document.dispatchEvent(new Event("visibilitychange"));
      });
      const sentActivate = ws.sent
        .slice(before)
        .find((m) => typeof m === "string" && m.includes('"activate"'));
      expect(sentActivate).toBeDefined();
    } finally {
      div.remove();
    }
  });

  it("'online' on a CLOSED socket dials a fresh WS", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      renderHook(() => {
        const term = useTerminal("s-online", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      // Force the WS into the CLOSED state so the autoreconnect path
      // is the only viable resolution.
      sockets[0]!.readyState = FakeWebSocket.CLOSED;
      const before = sockets.length;
      act(() => {
        window.dispatchEvent(new Event("online"));
      });
      await flushAsync();
      expect(sockets.length).toBeGreaterThan(before);
    } finally {
      div.remove();
    }
  });

  it("font-size effect updates term.options.fontSize when settings change", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    // Pre-seed a settings entry so the hook picks up a non-default
    // initial font size, then bump it via localStorage. The hook
    // re-reads settings via useWebSettings which is store-backed.
    try {
      localStorage.setItem(
        "aoe-web-settings",
        JSON.stringify({ mobileFontSize: 14, desktopFontSize: 22 }),
      );
      renderHook(() => {
        const term = useTerminal("s-font", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      // The constructor read 22 from localStorage.
      expect(captured.options.fontSize).toBe(22);
    } finally {
      localStorage.removeItem("aoe-web-settings");
      div.remove();
    }
  });

  it("single-finger touch swipe emits SGR wheel sequences to the WS", async () => {
    // The touch handler attaches to term.element with capture + passive
    // false. We dispatch synthetic TouchEvents through the captured
    // element to exercise the gesture-detection, scroll-accumulation,
    // and sendWheel paths.
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      renderHook(() => {
        const term = useTerminal("s-touch", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();

      // Grab the .xterm element our FakeTerminal created.
      const xtermEl = div.querySelector(".xterm") as HTMLDivElement;
      expect(xtermEl).toBeTruthy();

      const before = ws.sent.length;
      const fireTouch = (
        type: "touchstart" | "touchmove" | "touchend",
        y: number,
      ) => {
        // jsdom has no Touch constructor; build the event ourselves
        // and attach plain Touch-like objects through Object.defineProperty
        // so the hook's TouchEvent.touches reads see what we want.
        const ev = new Event(type, { bubbles: true, cancelable: true });
        const touches =
          type === "touchend"
            ? []
            : [{ clientX: 100, clientY: y, identifier: 1 }];
        Object.defineProperty(ev, "touches", { value: touches });
        Object.defineProperty(ev, "targetTouches", { value: touches });
        Object.defineProperty(ev, "changedTouches", {
          value: [{ clientX: 100, clientY: y, identifier: 1 }],
        });
        xtermEl.dispatchEvent(ev);
      };

      // Vertical swipe up: start, move up far enough to clear the
      // 12px gesture-lock, then accumulate enough delta to trigger
      // multiple wheel events.
      act(() => {
        fireTouch("touchstart", 500);
        // First move beyond GESTURE_LOCK_PX (12)
        fireTouch("touchmove", 480);
        // Subsequent moves drive sendWheel
        fireTouch("touchmove", 420);
        fireTouch("touchmove", 360);
        fireTouch("touchmove", 300);
        fireTouch("touchend", 300);
      });
      await flushAsync();

      const newSends = ws.sent.slice(before);
      // Look for any SGR wheel emission. up=`\x1b[<64...` down=`\x1b[<65...`
      const sgrWheel = newSends.some((m) => {
        if (typeof m === "string") return false;
        const ascii = Array.from(m)
          .map((b) => String.fromCharCode(b))
          .join("");
        return ascii.startsWith("\x1b[<64") || ascii.startsWith("\x1b[<65");
      });
      expect(sgrWheel).toBe(true);
    } finally {
      div.remove();
    }
  });

  it("two-finger touch pinch updates the font size", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      renderHook(() => {
        const term = useTerminal("s-pinch", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const xtermEl = div.querySelector(".xterm") as HTMLDivElement;
      const startSize = captured.options.fontSize;

      const twoFinger = (
        type: "touchstart" | "touchmove" | "touchend",
        dist: number,
      ) => {
        const t1 = { identifier: 1, clientX: 100 - dist / 2, clientY: 500 };
        const t2 = { identifier: 2, clientX: 100 + dist / 2, clientY: 500 };
        const ev = new Event(type, { bubbles: true, cancelable: true });
        const list = type === "touchend" ? [] : [t1, t2];
        Object.defineProperty(ev, "touches", { value: list });
        Object.defineProperty(ev, "targetTouches", { value: list });
        Object.defineProperty(ev, "changedTouches", { value: [t1, t2] });
        xtermEl.dispatchEvent(ev);
      };

      // Spread the fingers apart from 50px to 300px to zoom in.
      act(() => {
        twoFinger("touchstart", 50);
        twoFinger("touchmove", 80);
        twoFinger("touchmove", 150);
        twoFinger("touchmove", 250);
        twoFinger("touchmove", 300);
        twoFinger("touchend", 300);
      });
      // Flush the rAF for the font-size coalesce path.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(50);
      });
      expect(captured.options.fontSize).not.toBe(startSize);
    } finally {
      div.remove();
    }
  });

  it("ws.onclose writes the disconnect+retry banner to the terminal", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      renderHook(() => {
        const term = useTerminal("s-retry", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      const beforeWrites = captured.writes.length;
      act(() => {
        ws.readyState = FakeWebSocket.CLOSED;
        ws.onclose?.({
          code: 1006,
          reason: "boom",
          wasClean: false,
        } as CloseEvent);
      });
      // term.write should have printed the disconnect+reconnect banner
      const newWrites = captured.writes.slice(beforeWrites);
      const banner = newWrites.find(
        (w) =>
          typeof w === "string" &&
          w.includes("Disconnected") &&
          w.includes("reconnecting"),
      );
      expect(banner).toBeDefined();
    } finally {
      div.remove();
    }
  });

  it("ws.onerror logs through console without blowing up", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      renderHook(() => {
        const term = useTerminal("s-err", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      // Just confirm ws.onerror fires without throwing.
      expect(() => {
        ws.onerror?.(new Event("error"));
      }).not.toThrow();
    } finally {
      div.remove();
    }
  });

  it("ws.onclose at retries-exhausted prints the Connection-lost banner", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-exhaust", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      // Walk through all retries by closing each new socket in turn
      // until the retry counter reaches max. The final close path then
      // takes the retries-exhausted branch which prints the
      // Connection-lost banner.
      const maxRetries = result.current.maxRetries;
      let currentWs = ws;
      // maxRetries close cycles -> the (max+1)-th close should bypass
      // the retry path and land on the exhausted branch.
      for (let i = 0; i < maxRetries + 1; i++) {
        act(() => {
          currentWs.readyState = FakeWebSocket.CLOSED;
          currentWs.onclose?.({
            code: 1006,
            reason: "",
            wasClean: false,
          } as CloseEvent);
        });
        await act(async () => {
          await vi.advanceTimersByTimeAsync(60_000);
        });
        await flushAsync();
        if (sockets.length > i + 1) {
          currentWs = sockets[sockets.length - 1]!;
        }
      }
      // Final close should land on retries-exhausted.
      const banner = captured.writes.find(
        (w) => typeof w === "string" && w.includes("Connection lost"),
      );
      expect(banner).toBeDefined();
    } finally {
      div.remove();
    }
  });

  it("wheel-up over a quiet pane sets isInScrollback and sends pause_output", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-pause", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();
      const before = ws.sent.length;
      // Drive several wheel-up events so sendWheel runs the entry-into-
      // scrollback branch (sets isInScrollback + sends pause_output).
      // act() so the setState reducer side-effect (the ws.send for
      // pause_output) flushes inside the reducer's batched update.
      await act(async () => {
        for (let i = 0; i < 8; i++) {
          const e = new WheelEvent("wheel", {
            deltaY: -200,
            bubbles: true,
            cancelable: true,
          });
          captured.customWheel!(e);
        }
      });
      await flushAsync();
      const pause = ws.sent
        .slice(before)
        .find((m) => typeof m === "string" && m.includes('"pause_output"'));
      expect(pause).toBeDefined();
      expect(result.current.state.isInScrollback).toBe(true);
    } finally {
      div.remove();
    }
  });

  it("exitScrollback() sends resume_output + Escape and clears the flag", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-exit", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();
      // Get into scrollback first
      await act(async () => {
        for (let i = 0; i < 8; i++) {
          const e = new WheelEvent("wheel", {
            deltaY: -200,
            bubbles: true,
            cancelable: true,
          });
          captured.customWheel!(e);
        }
      });
      await flushAsync();
      expect(result.current.state.isInScrollback).toBe(true);

      const before = ws.sent.length;
      act(() => {
        result.current.exitScrollback();
      });
      const newSends = ws.sent.slice(before);
      const resume = newSends.find(
        (m) => typeof m === "string" && m.includes('"resume_output"'),
      );
      expect(resume).toBeDefined();
      // \x1b (ESC) should also have been sent
      const escByte = newSends.find((m) => {
        if (typeof m === "string") return false;
        return m.length === 1 && m[0] === 0x1b;
      });
      expect(escByte).toBeDefined();
      expect(result.current.state.isInScrollback).toBe(false);
    } finally {
      div.remove();
    }
  });

  it("the virtual Ctrl bridge transforms onData printable -> control byte", async () => {
    const div = document.createElement("div");
    document.body.appendChild(div);
    try {
      const { result } = renderHook(() => {
        const term = useTerminal("s-ctrl", "ws", false, false);
        if (term.containerRef && !term.containerRef.current) {
          (
            term.containerRef as unknown as { current: HTMLDivElement | null }
          ).current = div;
        }
        return term;
      });
      await flushAsync();
      const ws = sockets[0]!;
      act(() => {
        ws.readyState = FakeWebSocket.OPEN;
        ws.onopen?.(new Event("open"));
      });
      await flushAsync();
      // Arm the Ctrl bridge from React state.
      act(() => {
        result.current.ctrlActiveRef.current = true;
      });
      // Now simulate the user typing 'a' through xterm's onData.
      const before = ws.sent.length;
      act(() => {
        captured.onData?.("a");
      });
      // The hook should have sent 0x01 (Ctrl-A) instead of 'a'.
      const newSends = ws.sent.slice(before);
      const ctrlA = newSends.find((m) => {
        if (typeof m === "string") return false;
        return m.length === 1 && m[0] === 0x01;
      });
      expect(ctrlA).toBeDefined();
      // The ref should also be cleared after consumption.
      expect(result.current.ctrlActiveRef.current).toBe(false);
    } finally {
      div.remove();
    }
  });
});

// Mobile soft-keyboard Backspace autorepeat handler (#1450). The hook
// intercepts `beforeinput` on xterm's hidden textarea and emits one DEL
// (0x7f) per `deleteContentBackward` tick, gated to coarse-pointer devices
// without a fine pointer. jsdom lets us drive every branch deterministically:
// matchMedia controls the gate, the FakeSocket's readyState the open guard.
describe("useTerminal mobile backspace autorepeat", () => {
  function stubPointer(coarse: boolean, anyFine: boolean): void {
    vi.stubGlobal(
      "matchMedia",
      (q: string) =>
        ({
          matches:
            q === "(pointer: coarse)"
              ? coarse
              : q === "(any-pointer: fine)"
                ? anyFine
                : false,
        }) as MediaQueryList,
    );
  }

  // Mount the hook, attach a container, and drive the socket to OPEN so the
  // beforeinput handler's `ws.readyState === OPEN` guard passes. Returns the
  // textarea xterm's mock created and the live FakeSocket.
  async function mountOpen(): Promise<{
    div: HTMLDivElement;
    ws: FakeSocket;
    ta: HTMLTextAreaElement;
  }> {
    const div = document.createElement("div");
    document.body.appendChild(div);
    renderHook(() => {
      const term = useTerminal("s-bksp", "ws", false, false);
      if (term.containerRef && !term.containerRef.current) {
        (
          term.containerRef as unknown as { current: HTMLDivElement | null }
        ).current = div;
      }
      return term;
    });
    await flushAsync();
    const ws = sockets[0]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.(new Event("open"));
    });
    await flushAsync();
    const ta = div.querySelector("textarea") as HTMLTextAreaElement;
    return { div, ws, ta };
  }

  function fireDelete(
    ta: HTMLTextAreaElement,
    count: number,
    opts: { inputType?: string; isComposing?: boolean } = {},
  ): void {
    const inputType = opts.inputType ?? "deleteContentBackward";
    act(() => {
      for (let i = 0; i < count; i++) {
        const e = new InputEvent("beforeinput", {
          inputType,
          bubbles: true,
          cancelable: true,
        });
        if (opts.isComposing) {
          Object.defineProperty(e, "isComposing", { get: () => true });
        }
        ta.dispatchEvent(e);
      }
    });
  }

  function delBytes(ws: FakeSocket): number {
    return ws.sent.filter(
      (m) => typeof m !== "string" && m.length === 1 && m[0] === 0x7f,
    ).length;
  }

  it("emits one DEL per tick on a coarse pointer", async () => {
    stubPointer(true, false);
    const { div, ws, ta } = await mountOpen();
    try {
      fireDelete(ta, 3);
      expect(delBytes(ws)).toBe(3);
    } finally {
      div.remove();
    }
  });

  it("emits exactly one DEL for a single tick (no double-delete)", async () => {
    stubPointer(true, false);
    const { div, ws, ta } = await mountOpen();
    try {
      fireDelete(ta, 1);
      expect(delBytes(ws)).toBe(1);
    } finally {
      div.remove();
    }
  });

  it("does nothing on a fine pointer", async () => {
    stubPointer(false, false);
    const { div, ws, ta } = await mountOpen();
    try {
      fireDelete(ta, 3);
      expect(delBytes(ws)).toBe(0);
    } finally {
      div.remove();
    }
  });

  it("does nothing when a fine pointer is also present (iPad + keyboard)", async () => {
    stubPointer(true, true);
    const { div, ws, ta } = await mountOpen();
    try {
      fireDelete(ta, 3);
      expect(delBytes(ws)).toBe(0);
    } finally {
      div.remove();
    }
  });

  it("ignores non-delete input types", async () => {
    stubPointer(true, false);
    const { div, ws, ta } = await mountOpen();
    try {
      fireDelete(ta, 3, { inputType: "insertText" });
      expect(delBytes(ws)).toBe(0);
    } finally {
      div.remove();
    }
  });

  it("leaves composition deletes to xterm", async () => {
    stubPointer(true, false);
    const { div, ws, ta } = await mountOpen();
    try {
      fireDelete(ta, 3, { isComposing: true });
      expect(delBytes(ws)).toBe(0);
    } finally {
      div.remove();
    }
  });

  it("does not send while the socket is not open", async () => {
    stubPointer(true, false);
    const { div, ws, ta } = await mountOpen();
    try {
      act(() => {
        ws.readyState = FakeWebSocket.CLOSED;
      });
      fireDelete(ta, 3);
      expect(delBytes(ws)).toBe(0);
    } finally {
      div.remove();
    }
  });
});

// OSC 52 clipboard handling. tmux emits OSC 52 on copy (set-clipboard on);
// before #1499 xterm.js had no handler so select-to-copy silently failed.
// The hook now registers a handler that decodes the base64 payload and
// writes it to the system clipboard. These tests drive the captured handler
// directly; the full drag -> tmux -> OSC 52 round-trip is covered by the
// live Playwright spec.
describe("useTerminal OSC 52 clipboard", () => {
  let writeText: ReturnType<typeof vi.fn>;
  let originalClipboard: PropertyDescriptor | undefined;

  beforeEach(() => {
    writeText = vi.fn().mockResolvedValue(undefined);
    originalClipboard = Object.getOwnPropertyDescriptor(navigator, "clipboard");
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });
  });

  afterEach(() => {
    if (originalClipboard) {
      Object.defineProperty(navigator, "clipboard", originalClipboard);
    } else {
      delete (navigator as unknown as { clipboard?: unknown }).clipboard;
    }
  });

  async function mountHook(): Promise<HTMLDivElement> {
    const div = document.createElement("div");
    document.body.appendChild(div);
    renderHook(() => {
      const term = useTerminal("s-osc52", "ws", false, false);
      if (term.containerRef && !term.containerRef.current) {
        (
          term.containerRef as unknown as { current: HTMLDivElement | null }
        ).current = div;
      }
      return term;
    });
    await flushAsync();
    return div;
  }

  it("registers an OSC 52 handler on open", async () => {
    const div = await mountHook();
    try {
      expect(typeof captured.oscHandlers[52]).toBe("function");
    } finally {
      div.remove();
    }
  });

  it("decodes a base64 payload and writes it to the clipboard", async () => {
    const div = await mountHook();
    try {
      // "c;<base64>" where base64("hello world") = "aGVsbG8gd29ybGQ=".
      captured.oscHandlers[52]!("c;aGVsbG8gd29ybGQ=");
      await flushAsync();
      expect(writeText).toHaveBeenCalledWith("hello world");
    } finally {
      div.remove();
    }
  });

  it("swallows a rejected clipboard write", async () => {
    writeText.mockRejectedValueOnce(new Error("no gesture"));
    const div = await mountHook();
    try {
      expect(() => captured.oscHandlers[52]!("c;aGVsbG8=")).not.toThrow();
      await flushAsync();
      expect(writeText).toHaveBeenCalledWith("hello");
    } finally {
      div.remove();
    }
  });

  it("ignores an OSC 52 paste query (no clipboard write)", async () => {
    const div = await mountHook();
    try {
      captured.oscHandlers[52]!("c;?");
      await flushAsync();
      expect(writeText).not.toHaveBeenCalled();
    } finally {
      div.remove();
    }
  });

  it("ignores an undecodable payload without throwing", async () => {
    const div = await mountHook();
    try {
      expect(() =>
        captured.oscHandlers[52]!("c;!!!not base64!!!"),
      ).not.toThrow();
      await flushAsync();
      expect(writeText).not.toHaveBeenCalled();
    } finally {
      div.remove();
    }
  });

  // base64("hi") === "aGk=".
  const HI_OSC = "c;aGk=";

  // Stand-in for the browser ClipboardItem. A real ClipboardItem consumes
  // the promise value (it awaits it to perform the write), so it never leaks
  // an unhandled rejection when the hook's timeout rejects the pending blob.
  // The fake must do the same or the timeout tests trip Vitest's
  // unhandled-rejection guard (process exits non-zero even with all tests
  // green).
  class FakeClipboardItem {
    constructor(public data: Record<string, Promise<Blob>>) {
      for (const v of Object.values(data))
        void Promise.resolve(v).catch(() => {});
    }
  }

  function fireDrag(
    viewport: HTMLElement,
    from: { x: number; y: number },
    to: { x: number; y: number },
  ): void {
    viewport.dispatchEvent(
      new MouseEvent("mousedown", {
        button: 0,
        clientX: from.x,
        clientY: from.y,
        bubbles: true,
      }),
    );
    window.dispatchEvent(
      new MouseEvent("mouseup", {
        button: 0,
        clientX: to.x,
        clientY: to.y,
        bubbles: true,
      }),
    );
  }

  it("arms a ClipboardItem on drag release and resolves it from the OSC 52 payload", async () => {
    const write = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText, write },
      configurable: true,
    });
    vi.stubGlobal("ClipboardItem", FakeClipboardItem);
    const div = await mountHook();
    try {
      const viewport = div.querySelector(".xterm") as HTMLElement;
      fireDrag(viewport, { x: 10, y: 10 }, { x: 120, y: 90 });
      // The gesture pre-arms a promise-valued clipboard write.
      expect(write).toHaveBeenCalledTimes(1);
      // The OSC 52 escape resolves that pending write; the direct writeText
      // fallback is not used on the ClipboardItem path.
      captured.oscHandlers[52]!(HI_OSC);
      await flushAsync();
      expect(writeText).not.toHaveBeenCalled();
    } finally {
      div.remove();
    }
  });

  it("does not arm on a plain click (below the drag threshold)", async () => {
    const write = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText, write },
      configurable: true,
    });
    vi.stubGlobal("ClipboardItem", FakeClipboardItem);
    const div = await mountHook();
    try {
      const viewport = div.querySelector(".xterm") as HTMLElement;
      fireDrag(viewport, { x: 10, y: 10 }, { x: 11, y: 11 });
      expect(write).not.toHaveBeenCalled();
      // An unarmed OSC 52 (e.g. the agent ran a copy itself) still writes.
      captured.oscHandlers[52]!(HI_OSC);
      await flushAsync();
      expect(writeText).toHaveBeenCalledWith("hi");
    } finally {
      div.remove();
    }
  });

  it("falls back to writeText when promise ClipboardItem is unavailable", async () => {
    vi.stubGlobal("ClipboardItem", undefined);
    const div = await mountHook();
    try {
      const viewport = div.querySelector(".xterm") as HTMLElement;
      fireDrag(viewport, { x: 10, y: 10 }, { x: 120, y: 90 });
      captured.oscHandlers[52]!(HI_OSC);
      await flushAsync();
      expect(writeText).toHaveBeenCalledWith("hi");
    } finally {
      div.remove();
    }
  });

  it("clears the armed ClipboardItem write when no OSC 52 arrives in time", async () => {
    const write = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText, write },
      configurable: true,
    });
    vi.stubGlobal("ClipboardItem", FakeClipboardItem);
    const div = await mountHook();
    try {
      const viewport = div.querySelector(".xterm") as HTMLElement;
      fireDrag(viewport, { x: 10, y: 10 }, { x: 120, y: 90 });
      expect(write).toHaveBeenCalledTimes(1);
      // No OSC 52 lands: the timeout fires and disarms.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(600);
      });
      // A late OSC 52 no longer resolves the (expired) item; it writes direct.
      captured.oscHandlers[52]!(HI_OSC);
      await flushAsync();
      expect(writeText).toHaveBeenCalledWith("hi");
    } finally {
      div.remove();
    }
  });

  it("disarms the writeText fallback after its timeout", async () => {
    vi.stubGlobal("ClipboardItem", undefined);
    const div = await mountHook();
    try {
      const viewport = div.querySelector(".xterm") as HTMLElement;
      fireDrag(viewport, { x: 10, y: 10 }, { x: 120, y: 90 });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(600);
      });
      // After the timeout the fallback resolver is cleared; a later OSC 52
      // takes the direct-write path exactly once.
      captured.oscHandlers[52]!(HI_OSC);
      await flushAsync();
      expect(writeText).toHaveBeenCalledTimes(1);
      expect(writeText).toHaveBeenCalledWith("hi");
    } finally {
      div.remove();
    }
  });
});

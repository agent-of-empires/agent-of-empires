// @vitest-environment jsdom
//
// Covers the live-view pointer forwarding the mobile component relies on:
// `forwardWheel` asks the daemon to encode the notches, `forwardButton`
// encodes its own bytes, and incoming frames surface the pane's mouse flags.

import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { useLiveTerminal } from "./useLiveTerminal";

vi.mock("../lib/token", () => ({ getToken: () => null }));
vi.mock("../lib/deviceBinding", () => ({ getOrCreateDeviceBindingSecret: () => null }));

class FakeWS {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  static last: FakeWS | null = null;
  readyState = FakeWS.OPEN;
  onopen: ((e: unknown) => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onclose: ((e: unknown) => void) | null = null;
  sent: unknown[] = [];
  constructor(_url: string, _protocols?: string | string[]) {
    FakeWS.last = this;
  }
  send(d: unknown) {
    this.sent.push(d);
  }
  close() {
    this.readyState = FakeWS.CLOSED;
  }
}

beforeEach(() => {
  FakeWS.last = null;
  vi.stubGlobal("WebSocket", FakeWS as unknown as typeof WebSocket);
});

const sentBytes = (ws: FakeWS) => ws.sent.filter((d): d is Uint8Array => d instanceof Uint8Array);
const sentJson = (ws: FakeWS) => ws.sent.filter((d): d is string => typeof d === "string").map((d) => JSON.parse(d));

describe("useLiveTerminal forwardWheel", () => {
  // Raw input bytes are dropped for a viewer that does not hold the size
  // lock, so a wheel sent that way left a watcher unable to scroll at all.
  // The daemon takes this control message from any viewer and encodes it
  // against the pane's own modes.
  it("asks the daemon to encode the notches rather than sending bytes", () => {
    const { result } = renderHook(() => useLiveTerminal("s", "live-ws"));
    act(() => result.current.forwardWheel(true, 3, 4, 2));
    const ws = FakeWS.last!;
    expect(sentJson(ws)).toContainEqual({ type: "wheel", up: true, col: 3, row: 4, count: 2 });
    expect(sentBytes(ws).length).toBe(0);
  });

  it("stands for one notch by default and never for none", () => {
    const { result } = renderHook(() => useLiveTerminal("s", "live-ws"));
    const ws = FakeWS.last!;
    act(() => result.current.forwardWheel(false, 3, 3));
    expect(sentJson(ws)).toContainEqual({ type: "wheel", up: false, col: 3, row: 3, count: 1 });
    const before = ws.sent.length;
    act(() => result.current.forwardWheel(false, 3, 3, 0));
    expect(ws.sent.length).toBe(before);
  });

  it("does not send when the socket is not open", () => {
    const { result } = renderHook(() => useLiveTerminal("s", "live-ws"));
    const ws = FakeWS.last!;
    ws.readyState = FakeWS.CLOSED;
    ws.sent.length = 0;
    act(() => result.current.forwardWheel(true, 3, 3));
    expect(ws.sent.length).toBe(0);
  });

  it("sends SGR button press / drag / release bytes", () => {
    const { result } = renderHook(() => useLiveTerminal("s", "live-ws"));
    const ws = FakeWS.last!;
    const decode = (b: Uint8Array) => new TextDecoder().decode(b);
    act(() => result.current.forwardButton(0, false, false, true, 4, 2)); // left press
    act(() => result.current.forwardButton(0, false, true, true, 5, 2)); // drag
    act(() => result.current.forwardButton(0, true, false, true, 6, 2)); // release
    const seen = sentBytes(ws).map(decode);
    expect(seen).toContain("\x1b[<0;4;2M");
    expect(seen).toContain("\x1b[<32;5;2M");
    expect(seen).toContain("\x1b[<0;6;2m");
  });

  it("does not send a button when the socket is not open", () => {
    const { result } = renderHook(() => useLiveTerminal("s", "live-ws"));
    const ws = FakeWS.last!;
    ws.readyState = FakeWS.CLOSED;
    ws.sent.length = 0;
    act(() => result.current.forwardButton(0, false, false, true, 1, 1));
    expect(sentBytes(ws).length).toBe(0);
  });

  it("surfaces the pane's mouse flags from incoming frames", () => {
    const { result } = renderHook(() => useLiveTerminal("s", "live-ws"));
    const ws = FakeWS.last!;
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          type: "frame",
          content: "x\n",
          rows: 1,
          history: 0,
          cursor: null,
          altScreen: true,
          mouse: true,
          mouseSgr: false,
          mouseAll: true,
          pane0: { cols: 40, rows: 24, left: 0, top: 1 },
        }),
      });
    });
    expect(result.current.state.frame?.altScreen).toBe(true);
    expect(result.current.state.frame?.mouse).toBe(true);
    expect(result.current.state.frame?.mouseSgr).toBe(false);
    expect(result.current.state.frame?.mouseAll).toBe(true);
    expect(result.current.state.frame?.pane0).toEqual({ cols: 40, rows: 24, left: 0, top: 1 });
  });

  // Both were published by the daemon and read by nothing here, so the
  // dashboard could not name who took the pane and duplicated the window
  // ceiling as a literal.
  it("reads the lock holder and the daemon's window ceiling", () => {
    const { result } = renderHook(() => useLiveTerminal("s", "live-ws"));
    const ws = FakeWS.last!;
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({ type: "size_owner", is_owner: false, holder: "mac-mini (aoe)" }),
      });
      ws.onmessage?.({ data: JSON.stringify({ type: "transport", grid: true, maxWindow: 9000 }) });
    });
    expect(result.current.state.holder).toBe("mac-mini (aoe)");
    expect(result.current.state.maxWindow).toBe(9000);

    // Owning it again clears the holder rather than leaving a stale name.
    act(() => {
      ws.onmessage?.({ data: JSON.stringify({ type: "size_owner", is_owner: true }) });
    });
    expect(result.current.state.holder).toBe(null);
  });

  it("delivers every clipboard event even when the copied text repeats", () => {
    const onClipboard = vi.fn();
    renderHook(() => useLiveTerminal("s", "live-ws", onClipboard));
    const ws = FakeWS.last!;
    act(() => {
      ws.onmessage?.({ data: JSON.stringify({ type: "clipboard", text: "same text" }) });
    });
    act(() => {
      ws.onmessage?.({ data: JSON.stringify({ type: "clipboard", text: "same text" }) });
    });
    expect(onClipboard).toHaveBeenNthCalledWith(1, "same text");
    expect(onClipboard).toHaveBeenNthCalledWith(2, "same text");
  });

  it("uses the latest clipboard callback without reconnecting", () => {
    const first = vi.fn();
    const second = vi.fn();
    const { rerender } = renderHook(({ onClipboard }) => useLiveTerminal("s", "live-ws", onClipboard), {
      initialProps: { onClipboard: first },
    });
    const ws = FakeWS.last!;

    rerender({ onClipboard: second });
    act(() => {
      ws.onmessage?.({ data: JSON.stringify({ type: "clipboard", text: "updated callback" }) });
    });

    expect(FakeWS.last).toBe(ws);
    expect(first).not.toHaveBeenCalled();
    expect(second).toHaveBeenCalledWith("updated callback");
  });
});

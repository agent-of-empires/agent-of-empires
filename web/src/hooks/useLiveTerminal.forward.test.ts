// @vitest-environment jsdom
//
// Covers the live-view wheel forwarding the mobile component relies on:
// `forwardWheel` emits the right bytes over the socket (SGR vs legacy),
// and incoming frames surface the altScreen / mouse / mouseSgr flags.

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

describe("useLiveTerminal forwardWheel", () => {
  it("sends SGR wheel bytes when the app is in SGR encoding", () => {
    const { result } = renderHook(() => useLiveTerminal("s", "live-ws"));
    act(() => result.current.forwardWheel(true, true, 3, 3));
    const ws = FakeWS.last!;
    const hit = sentBytes(ws).some((b) => new TextDecoder().decode(b) === "\x1b[<64;3;3M");
    expect(hit).toBe(true);
  });

  it("sends legacy X10 wheel bytes when SGR is off", () => {
    const { result } = renderHook(() => useLiveTerminal("s", "live-ws"));
    act(() => result.current.forwardWheel(false, false, 3, 3));
    const ws = FakeWS.last!;
    const hit = sentBytes(ws).some(
      (b) => b.length === 6 && b[0] === 0x1b && b[1] === 0x5b && b[2] === 0x4d && b[3] === 0x61,
    );
    expect(hit).toBe(true);
  });

  it("does not send when the socket is not open", () => {
    const { result } = renderHook(() => useLiveTerminal("s", "live-ws"));
    const ws = FakeWS.last!;
    ws.readyState = FakeWS.CLOSED;
    ws.sent.length = 0;
    act(() => result.current.forwardWheel(true, true, 3, 3));
    expect(sentBytes(ws).length).toBe(0);
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

  it("surfaces altScreen / mouse / mouseSgr from incoming frames", () => {
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
        }),
      });
    });
    expect(result.current.state.frame?.altScreen).toBe(true);
    expect(result.current.state.frame?.mouse).toBe(true);
    expect(result.current.state.frame?.mouseSgr).toBe(false);
  });
});

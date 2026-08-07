// @vitest-environment jsdom
//
// Size-owner exclusivity for the mobile live view. Only one client at a
// time may drive a session's size and type into it across every surface
// (web PTY attach, mobile live, native TUI); a non-owner renders
// best-effort and shows a "take over" affordance. These tests pin the
// wire contract the server (src/server/live_ws.rs) relies on: the client
// honors `{"type":"size_owner","is_owner":..}` frames, gates input on
// ownership, and emits `{"type":"claim"}` on an explicit take-over.

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useLiveTerminal } from "./useLiveTerminal";

vi.mock("../lib/token", () => ({ getToken: () => null }));
vi.mock("../lib/deviceBinding", () => ({
  getOrCreateDeviceBindingSecret: () => "test-secret",
}));

interface FakeSocket {
  readyState: number;
  onopen: ((ev: Event) => void) | null;
  onclose: ((ev: CloseEvent) => void) | null;
  onmessage: ((ev: MessageEvent) => void) | null;
  sent: Array<string | Uint8Array>;
}

const sockets: FakeSocket[] = [];
let originalWebSocket: typeof WebSocket;

class FakeWebSocket implements FakeSocket {
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
  constructor(_url: string, _protocols?: string | string[]) {
    sockets.push(this);
  }
  close(): void {
    this.readyState = FakeWebSocket.CLOSED;
  }
  send(data: string | ArrayBufferLike | Blob | ArrayBufferView): void {
    if (typeof data === "string") this.sent.push(data);
    else this.sent.push(new Uint8Array(data as ArrayBuffer));
  }
}

function open(socket: FakeSocket) {
  socket.readyState = FakeWebSocket.OPEN;
  act(() => socket.onopen?.({} as Event));
}

function deliver(socket: FakeSocket, payload: unknown) {
  act(() => socket.onmessage?.({ data: JSON.stringify(payload) } as MessageEvent));
}

beforeEach(() => {
  sockets.length = 0;
  originalWebSocket = global.WebSocket;
  global.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
});

afterEach(() => {
  global.WebSocket = originalWebSocket;
});

describe("useLiveTerminal size-owner", () => {
  it("waits for a size_owner frame before allowing input", () => {
    const { result } = renderHook(() => useLiveTerminal("s1"));
    const socket = sockets[0];
    open(socket);
    expect(result.current.state.isOwner).toBe(false);
    expect(result.current.state.ownerKnown).toBe(false);
    expect(socket.sent).toContain(JSON.stringify({ type: "claim_if_vacant" }));
  });

  it("becomes read-only on size_owner false and back on true", () => {
    const { result } = renderHook(() => useLiveTerminal("s1"));
    const socket = sockets[0];
    open(socket);

    deliver(socket, { type: "size_owner", is_owner: false });
    expect(result.current.state.isOwner).toBe(false);
    expect(result.current.state.ownerKnown).toBe(true);

    deliver(socket, { type: "size_owner", is_owner: true });
    expect(result.current.state.isOwner).toBe(true);
  });

  it("drops input after ownership is denied instead of sending it after a later takeover", () => {
    const { result } = renderHook(() => useLiveTerminal("s1"));
    const socket = sockets[0];
    open(socket);
    socket.sent.length = 0;

    deliver(socket, { type: "size_owner", is_owner: false });
    act(() => result.current.sendData("x"));
    expect(socket.sent).toHaveLength(0);

    deliver(socket, { type: "size_owner", is_owner: true });
    expect(socket.sent.some((m) => m instanceof Uint8Array)).toBe(false);
    act(() => result.current.sendData("y"));
    const typed = socket.sent.find((m): m is Uint8Array => m instanceof Uint8Array);
    expect(new TextDecoder().decode(typed)).toBe("y");
  });

  it("queues the first typed bytes until a newly selected session owns the pane", () => {
    const { result } = renderHook(() => useLiveTerminal("s1"));
    const socket = sockets[0];
    act(() => result.current.sendData("first"));
    expect(socket.sent).toHaveLength(0);

    open(socket);
    expect(socket.sent.some((m) => m instanceof Uint8Array)).toBe(false);

    deliver(socket, { type: "size_owner", is_owner: true });
    const typed = socket.sent.find((m): m is Uint8Array => m instanceof Uint8Array);
    expect(typed).toBeDefined();
    expect(new TextDecoder().decode(typed)).toBe("first");
  });

  it("emits a claim message on explicit take-over", () => {
    const { result } = renderHook(() => useLiveTerminal("s1"));
    const socket = sockets[0];
    open(socket);
    socket.sent.length = 0;

    act(() => result.current.claim());
    expect(socket.sent).toContain(JSON.stringify({ type: "claim" }));
  });

  it("keeps reporting its grid (resize) even while a non-owner so a vacated lock can be reclaimed", () => {
    const { result } = renderHook(() => useLiveTerminal("s1"));
    const socket = sockets[0];
    open(socket);

    deliver(socket, { type: "size_owner", is_owner: false });
    socket.sent.length = 0;
    act(() => result.current.sendResize(52, 20));
    expect(socket.sent).toContain(JSON.stringify({ type: "resize", cols: 52, rows: 20 }));
  });

  it("ignores a superseded socket closing after its replacement owns the pane", () => {
    const { result } = renderHook(() => useLiveTerminal("s1"));
    const oldSocket = sockets[0];
    open(oldSocket);

    oldSocket.readyState = FakeWebSocket.CLOSED;
    act(() => window.dispatchEvent(new Event("online")));
    const replacement = sockets[1];
    open(replacement);
    deliver(replacement, { type: "size_owner", is_owner: true });

    act(() => oldSocket.onclose?.(new CloseEvent("close")));
    expect(result.current.state.isOwner).toBe(true);
    expect(result.current.state.ownerKnown).toBe(true);
  });

  it("resets ownership when the active socket closes", () => {
    vi.useFakeTimers();
    try {
      const { result } = renderHook(() => useLiveTerminal("s1"));
      const socket = sockets[0];
      open(socket);
      deliver(socket, { type: "size_owner", is_owner: true });

      act(() => socket.onclose?.(new CloseEvent("close")));
      expect(result.current.state.isOwner).toBe(false);
      expect(result.current.state.ownerKnown).toBe(false);
    } finally {
      vi.clearAllTimers();
      vi.useRealTimers();
    }
  });
});

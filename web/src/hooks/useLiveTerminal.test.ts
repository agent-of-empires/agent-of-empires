// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { applyPatch, frameLines, useLiveTerminal } from "./useLiveTerminal";

vi.mock("../lib/token", () => ({ getToken: () => null }));
vi.mock("../lib/deviceBinding", () => ({ getOrCreateDeviceBindingSecret: () => "test-secret" }));

class FakeWS {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  static sockets: FakeWS[] = [];
  readyState = FakeWS.CONNECTING;
  binaryType = "blob";
  onopen: ((e: Event) => void) | null = null;
  onmessage: ((e: MessageEvent) => void) | null = null;
  onclose: ((e: CloseEvent) => void) | null = null;
  onerror: ((e: Event) => void) | null = null;
  sent: Array<string | Uint8Array> = [];
  constructor() {
    FakeWS.sockets.push(this);
  }
  send(data: string | ArrayBufferLike | ArrayBufferView) {
    this.sent.push(typeof data === "string" ? data : new Uint8Array(data as ArrayBuffer));
  }
  close() {
    this.readyState = FakeWS.CLOSED;
  }
  open() {
    this.readyState = FakeWS.OPEN;
    act(() => this.onopen?.({} as Event));
  }
  deliver(payload: unknown) {
    act(() => this.onmessage?.({ data: JSON.stringify(payload) } as MessageEvent));
  }
  json() {
    return this.sent
      .filter((d): d is string => typeof d === "string")
      .map((d) => JSON.parse(d) as Record<string, unknown>);
  }
  bytes() {
    return this.sent.filter((d): d is Uint8Array => d instanceof Uint8Array).map((b) => new TextDecoder().decode(b));
  }
}

beforeEach(() => {
  FakeWS.sockets.length = 0;
  vi.stubGlobal("WebSocket", FakeWS);
});

/** Render the hook and return it with its (optionally opened) socket. */
function mount(openSocket = true) {
  const hook = renderHook(() => useLiveTerminal("s1", "live-ws"));
  const ws = FakeWS.sockets[0]!;
  if (openSocket) ws.open();
  return { ...hook, ws };
}

const frame = (seq: number, content: string) => ({ type: "frame", seq, content, rows: 3, history: 5, cursor: null });
const patch = (seq: number, base: number, lines: [number, string][], shift = 0) => ({
  type: "patch",
  seq,
  base,
  shift,
  lines,
  rows: 3,
  history: 5,
});

describe("row patches", () => {
  it("advertises patch support and applies a patch onto the held frame", () => {
    const { result, ws } = mount();
    expect(ws.json().find((m) => m.type === "caps")).toMatchObject({ patch: true });

    ws.deliver(frame(1, "a\nb\nc\n"));
    ws.deliver({ ...patch(2, 1, [[2, "d"]], 1), history: 6, cursor: { x: 0, y: 2 } });
    expect(result.current.state.frame).toMatchObject({
      lines: ["b", "c", "d"],
      content: "b\nc\nd\n",
      history: 6,
      seq: 2,
    });
    expect(result.current.state.stats).toMatchObject({ frames: 1, patches: 1, resyncs: 0 });
  });

  it("requests one resync and ignores patches until a full frame lands", () => {
    const { result, ws } = mount();
    ws.deliver(frame(1, "a\nb\nc\n"));
    ws.deliver(patch(8, 7, [[0, "z"]]));
    ws.deliver(patch(9, 7, [[0, "z"]]));
    expect(result.current.state.frame?.lines).toEqual(["a", "b", "c"]);
    expect(ws.json().filter((m) => m.type === "resync")).toHaveLength(1);

    ws.deliver(frame(10, "x\ny\nz\n"));
    expect(result.current.state.stats.resyncs).toBe(1);
    ws.deliver(patch(11, 10, [[1, "Y"]]));
    expect(result.current.state.frame?.lines).toEqual(["x", "Y", "z"]);
  });

  it.each([
    ["a\nb\n", ["a", "b"]],
    ["a\n\n", ["a", ""]],
    ["", [""]],
  ])("frameLines(%j)", (content, expected) => {
    expect(frameLines(content)).toEqual(expected);
  });

  it.each<[string[], number, [number, string][], string[]]>([
    [["a", "b", "c"], 0, [[1, "B"]], ["a", "B", "c"]],
    [
      ["a", "b", "c"],
      2,
      [
        [1, "x"],
        [2, "y"],
      ],
      ["c", "x", "y"],
    ],
    [["a", "b"], 9, [], ["", ""]],
    [
      ["a", "b"],
      0,
      [
        [5, "q"],
        [-1, "r"],
      ],
      ["a", "b"],
    ],
  ])("applyPatch(%j, shift %i)", (prev, shift, changed, expected) => {
    expect(applyPatch(prev, shift, changed)).toEqual(expected);
  });
});

describe("mouse forwarding", () => {
  it.each([
    ["SGR wheel", (h: ReturnType<typeof useLiveTerminal>) => h.forwardWheel(true, true, 3, 3), ["\x1b[<64;3;3M"]],
    ["legacy X10 wheel", (h: ReturnType<typeof useLiveTerminal>) => h.forwardWheel(false, false, 3, 3), ["\x1b[Ma##"]],
    [
      "SGR press, drag, release",
      (h: ReturnType<typeof useLiveTerminal>) => {
        h.forwardButton(0, false, false, true, 4, 2);
        h.forwardButton(0, false, true, true, 5, 2);
        h.forwardButton(0, true, false, true, 6, 2);
      },
      ["\x1b[<0;4;2M", "\x1b[<32;5;2M", "\x1b[<0;6;2m"],
    ],
  ])("sends %s bytes, and nothing on a closed socket", (_label, send, expected) => {
    const { result, ws } = mount();
    ws.sent.length = 0;
    act(() => send(result.current));
    expect(ws.bytes()).toEqual(expected);

    ws.readyState = FakeWS.CLOSED;
    ws.sent.length = 0;
    act(() => send(result.current));
    expect(ws.bytes()).toEqual([]);
  });

  it("surfaces altScreen / mouse / mouseSgr / pane0 from frames", () => {
    const { result, ws } = mount();
    const pane0 = { cols: 40, rows: 24, left: 0, top: 1 };
    ws.deliver({ ...frame(1, "x\n"), altScreen: true, mouse: true, mouseSgr: false, pane0 });
    expect(result.current.state.frame).toMatchObject({ altScreen: true, mouse: true, mouseSgr: false, pane0 });
  });
});

describe("clipboard", () => {
  it("delivers every event, even repeated text, to the latest callback without reconnecting", () => {
    const first = vi.fn();
    const second = vi.fn();
    const { rerender } = renderHook(({ cb }) => useLiveTerminal("s", "live-ws", cb), { initialProps: { cb: first } });
    const ws = FakeWS.sockets[0]!;
    ws.deliver({ type: "clipboard", text: "same" });
    rerender({ cb: second });
    ws.deliver({ type: "clipboard", text: "same" });
    ws.deliver({ type: "clipboard", text: "same" });
    expect(FakeWS.sockets).toHaveLength(1);
    expect(first).toHaveBeenCalledTimes(1);
    expect(second).toHaveBeenCalledTimes(2);
  });
});

describe("size owner", () => {
  it("claims a vacant lock on open and waits for the owner verdict", () => {
    const { result, ws } = mount();
    expect(result.current.state).toMatchObject({ isOwner: false, ownerKnown: false });
    expect(ws.json()).toContainEqual({ type: "claim_if_vacant" });

    ws.deliver({ type: "size_owner", is_owner: false });
    expect(result.current.state).toMatchObject({ isOwner: false, ownerKnown: true });
    ws.deliver({ type: "size_owner", is_owner: true });
    expect(result.current.state.isOwner).toBe(true);
  });

  it("drops input once ownership is denied instead of sending it after a later takeover", () => {
    const { result, ws } = mount();
    ws.deliver({ type: "size_owner", is_owner: false });
    let accepted: boolean | undefined;
    act(() => {
      accepted = result.current.sendData("x");
    });
    expect(accepted).toBe(false);

    ws.deliver({ type: "size_owner", is_owner: true });
    expect(ws.bytes()).toEqual([]);
    act(() => {
      accepted = result.current.sendData("y");
    });
    expect(accepted).toBe(true);
    expect(ws.bytes()).toEqual(["y"]);
  });

  it("queues the first typed bytes until a newly selected session owns the pane", () => {
    const { result, ws } = mount(false);
    let accepted: boolean | undefined;
    act(() => {
      accepted = result.current.sendData("first");
    });
    expect(accepted).toBe(true);
    ws.open();
    expect(ws.bytes()).toEqual([]);
    ws.deliver({ type: "size_owner", is_owner: true });
    expect(ws.bytes()).toEqual(["first"]);
  });

  it("claims on take-over and keeps reporting its grid while a non-owner", () => {
    const { result, ws } = mount();
    ws.deliver({ type: "size_owner", is_owner: false });
    ws.sent.length = 0;
    act(() => result.current.claim());
    act(() => result.current.sendResize(52, 20));
    expect(ws.json()).toEqual(expect.arrayContaining([{ type: "claim" }, { type: "resize", cols: 52, rows: 20 }]));
  });

  it("ignores a superseded socket closing after its replacement owns the pane", () => {
    const { result, ws } = mount();
    ws.readyState = FakeWS.CLOSED;
    act(() => window.dispatchEvent(new Event("online")));
    const replacement = FakeWS.sockets[1]!;
    replacement.open();
    replacement.deliver({ type: "size_owner", is_owner: true });
    act(() => ws.onclose?.(new CloseEvent("close")));
    expect(result.current.state).toMatchObject({ isOwner: true, ownerKnown: true });
  });

  it("resets ownership when the active socket closes", () => {
    vi.useFakeTimers();
    try {
      const { result, ws } = mount();
      ws.deliver({ type: "size_owner", is_owner: true });
      act(() => ws.onclose?.(new CloseEvent("close")));
      expect(result.current.state).toMatchObject({ isOwner: false, ownerKnown: false });
    } finally {
      vi.clearAllTimers();
      vi.useRealTimers();
    }
  });
});

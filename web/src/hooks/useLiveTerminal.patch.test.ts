// @vitest-environment jsdom
//
// Row patches: the server sends only changed rows once the client advertises
// `caps.patch`. The hook must apply them against the frame it holds, keep the
// rendered row array in step, and ask for a full frame when continuity breaks.

import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { applyPatch, frameLines, useLiveTerminal } from "./useLiveTerminal";

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

const sentJson = (ws: FakeWS) =>
  ws.sent.filter((d): d is string => typeof d === "string").map((d) => JSON.parse(d) as Record<string, unknown>);

function frameMsg(seq: number, content: string) {
  return JSON.stringify({ type: "frame", seq, content, rows: 3, history: 5, cursor: null });
}

describe("useLiveTerminal row patches", () => {
  it("advertises patch support and applies a patch onto the held frame", () => {
    const { result } = renderHook(() => useLiveTerminal("s", "live-ws"));
    const ws = FakeWS.last!;
    act(() => ws.onopen?.({}));
    expect(sentJson(ws).find((m) => m.type === "caps")).toMatchObject({ patch: true });

    act(() => ws.onmessage?.({ data: frameMsg(1, "a\nb\nc\n") }));
    expect(result.current.state.frame?.lines).toEqual(["a", "b", "c"]);

    // History grew by one and the new tail row differs: drop the top row,
    // pad, then replace row 2.
    act(() =>
      ws.onmessage?.({
        data: JSON.stringify({
          type: "patch",
          seq: 2,
          base: 1,
          shift: 1,
          lines: [[2, "d"]],
          rows: 3,
          history: 6,
          cursor: { x: 0, y: 2 },
        }),
      }),
    );
    const frame = result.current.state.frame!;
    expect(frame.lines).toEqual(["b", "c", "d"]);
    expect(frame.content).toBe("b\nc\nd\n");
    expect(frame.history).toBe(6);
    expect(frame.seq).toBe(2);
    expect(result.current.state.stats).toMatchObject({ frames: 1, patches: 1, resyncs: 0 });
  });

  it("requests one resync and ignores patches until a full frame lands", () => {
    const { result } = renderHook(() => useLiveTerminal("s", "live-ws"));
    const ws = FakeWS.last!;
    act(() => ws.onopen?.({}));
    act(() => ws.onmessage?.({ data: frameMsg(1, "a\nb\nc\n") }));
    const stale = (seq: number) =>
      JSON.stringify({ type: "patch", seq, base: 7, shift: 0, lines: [[0, "z"]], rows: 3, history: 5 });
    act(() => ws.onmessage?.({ data: stale(8) }));
    act(() => ws.onmessage?.({ data: stale(9) }));
    expect(result.current.state.frame?.lines).toEqual(["a", "b", "c"]);
    expect(sentJson(ws).filter((m) => m.type === "resync")).toHaveLength(1);

    act(() => ws.onmessage?.({ data: frameMsg(10, "x\ny\nz\n") }));
    expect(result.current.state.frame?.lines).toEqual(["x", "y", "z"]);
    expect(result.current.state.stats.resyncs).toBe(1);
    // Continuity restored: the next well-based patch applies.
    act(() =>
      ws.onmessage?.({
        data: JSON.stringify({ type: "patch", seq: 11, base: 10, shift: 0, lines: [[1, "Y"]], rows: 3, history: 5 }),
      }),
    );
    expect(result.current.state.frame?.lines).toEqual(["x", "Y", "z"]);
  });
});

describe("patch helpers", () => {
  it("splits rows without the terminating newline", () => {
    expect(frameLines("a\nb\n")).toEqual(["a", "b"]);
    expect(frameLines("a\n\n")).toEqual(["a", ""]);
    expect(frameLines("")).toEqual([""]);
  });

  it("slides the window and replaces rows, ignoring out-of-range indices", () => {
    expect(applyPatch(["a", "b", "c"], 0, [[1, "B"]])).toEqual(["a", "B", "c"]);
    expect(
      applyPatch(["a", "b", "c"], 2, [
        [1, "x"],
        [2, "y"],
      ]),
    ).toEqual(["c", "x", "y"]);
    expect(applyPatch(["a", "b"], 9, [])).toEqual(["", ""]);
    expect(
      applyPatch(["a", "b"], 0, [
        [5, "q"],
        [-1, "r"],
      ]),
    ).toEqual(["a", "b"]);
  });
});

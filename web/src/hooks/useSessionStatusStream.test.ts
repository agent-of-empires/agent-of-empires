// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useSessionStatusStream } from "./useSessionStatusStream";

class FakeSocket {
  static all: FakeSocket[] = [];
  onmessage: ((e: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  close = vi.fn();
  constructor(
    readonly url: string,
    readonly protocols: string[],
  ) {
    FakeSocket.all.push(this);
  }
  push(frame: unknown) {
    this.onmessage?.({ data: typeof frame === "string" ? frame : JSON.stringify(frame) });
  }
}

describe("useSessionStatusStream", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    FakeSocket.all = [];
    vi.stubGlobal("WebSocket", FakeSocket);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  function mount() {
    const handlers = { onStatus: vi.fn(), onSoundDue: vi.fn(), onResync: vi.fn() };
    const view = renderHook(() => useSessionStatusStream(handlers));
    return { handlers, ...view };
  }

  it("dispatches frames, resyncs on ready and resync, and ignores malformed or unknown frames", () => {
    const { handlers } = mount();
    const ws = FakeSocket.all[0]!;
    expect(ws.url).toMatch(/\/api\/sessions\/status\/ws$/);
    expect(ws.protocols[0]).toBe("aoe-auth");

    act(() => {
      ws.push({ kind: "ready" });
      ws.push({ kind: "status", id: "a", status: "Waiting" });
      ws.push({ kind: "sound_due", id: "a", event: "waiting" });
      ws.push({ kind: "sound_due", id: "a", event: "bogus" });
      ws.push({ kind: "status", id: "a" });
      ws.push("not json");
      ws.push({ kind: "resync" });
    });

    expect(handlers.onResync).toHaveBeenCalledTimes(2);
    expect(handlers.onStatus).toHaveBeenCalledOnce();
    expect(handlers.onStatus).toHaveBeenCalledWith("a", "Waiting");
    expect(handlers.onSoundDue).toHaveBeenCalledOnce();
    expect(handlers.onSoundDue).toHaveBeenCalledWith("a", "waiting");
  });

  it("reconnects after a close and stops on unmount", () => {
    const { unmount } = mount();
    act(() => FakeSocket.all[0]!.onclose?.());
    expect(FakeSocket.all).toHaveLength(1);
    act(() => void vi.advanceTimersByTime(200));
    expect(FakeSocket.all).toHaveLength(2);

    unmount();
    act(() => FakeSocket.all[1]!.onclose?.());
    act(() => void vi.advanceTimersByTime(60_000));
    expect(FakeSocket.all).toHaveLength(2);
    expect(FakeSocket.all[1]!.close).toHaveBeenCalled();
  });

  it("drops a message that was already queued when it unmounted", () => {
    const { handlers, unmount } = mount();
    const ws = FakeSocket.all[0]!;
    unmount();
    act(() => ws.push({ kind: "status", id: "a", status: "Waiting" }));
    expect(handlers.onStatus).not.toHaveBeenCalled();
  });
});

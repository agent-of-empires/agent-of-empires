// @vitest-environment jsdom
// Wheel, touch, and click forwarding to a full-screen mouse app (altScreen && mouse). Byte encodings live in
// lib/__tests__/liveMouse.test.ts.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent } from "@testing-library/react";
import type { LiveFrame } from "../../hooks/useLiveTerminal";
import { deliverMobileKeyboardProxyInput } from "../../lib/mobileKeyboardProxy";
import { alt, installResizeObserver, liveFrame, renderLiveTerminal } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));
installResizeObserver();

type Mock = ReturnType<typeof vi.fn>;
const frame = (over: Partial<LiveFrame> = {}) => liveFrame({ content: "a\nb\nc\n", ...over });

function term(over: Partial<LiveFrame> = alt) {
  const view = renderLiveTerminal({ frame: frame(over) });
  return { ...view, wheel: view.props.forwardWheel as Mock, button: view.props.forwardButton as Mock };
}
const touch = (y: number, x = 100) => ({ clientX: x, clientY: y }) as Touch;
const mouse = (over: Record<string, unknown> = {}) => ({
  pointerType: "mouse",
  button: 0,
  clientX: 10,
  clientY: 10,
  ...over,
});
function drag(scroller: HTMLElement, from: number, to: number) {
  fireEvent.touchStart(scroller, { touches: [touch(from)] });
  fireEvent.touchMove(scroller, { touches: [touch(to)] });
}

describe("MobileLiveTerminal wheel forwarding", () => {
  it("forwards the wheel to a full-screen mouse app and pins the live edge", () => {
    const { scroller, wheel } = term();
    expect(scroller.className).toContain("overflow-hidden");
    fireEvent.wheel(scroller, { deltaY: 120 });
    // Down with SGR encoding.
    expect(wheel.mock.calls[0]!.slice(0, 2)).toEqual([false, true]);
    fireEvent.wheel(scroller, { deltaY: -120 });
    expect(wheel.mock.calls.at(-1)![0]).toBe(true);
    // A line-mode delta still forwards a notch.
    wheel.mockClear();
    fireEvent.wheel(scroller, { deltaY: 3, deltaMode: 1 });
    expect(wheel).toHaveBeenCalled();
  });

  it.each([
    ["an app with no mouse mode", { altScreen: true, mouse: false }],
    ["a normal-screen agent", { altScreen: false, mouse: true, mouseSgr: true }],
  ])("keeps capture scrolling for %s", (_n, over) => {
    const { scroller, wheel, button } = term(over);
    expect(scroller.className).toContain("overflow-y-auto");
    fireEvent.wheel(scroller, { deltaY: 120 });
    fireEvent.pointerDown(scroller, mouse());
    expect(wheel).not.toHaveBeenCalled();
    expect(button).not.toHaveBeenCalled();
  });

  it("owns touches with touch-action none and a native preventDefault only in forward mode", () => {
    // React's touch listeners are passive, so touch-action is what stops the page pan.
    for (const [over, action, prevented] of [
      [alt, "none", true],
      [{}, "", false],
    ] as const) {
      const { scroller, unmount } = term(over);
      expect(scroller.style.touchAction).toBe(action);
      const move = new Event("touchmove", { cancelable: true });
      scroller.dispatchEvent(move);
      expect(move.defaultPrevented).toBe(prevented);
      unmount();
    }
  });

  it("forwards a finger drag, including one iOS coalesces into touchend, as wheel down", () => {
    const moved = term();
    drag(moved.scroller, 300, 220);
    fireEvent.touchEnd(moved.scroller, { touches: [] });
    expect(moved.wheel.mock.calls[0]![0]).toBe(false);
    moved.unmount();

    const coalesced = term();
    fireEvent.touchStart(coalesced.scroller, { touches: [touch(300)] });
    fireEvent.touchEnd(coalesced.scroller, { touches: [], changedTouches: [touch(220)] });
    expect(coalesced.wheel.mock.calls[0]![0]).toBe(false);
  });

  it("does not turn a forward-mode swipe into a keyboard-opening click", () => {
    const { scroller, input } = term();
    drag(scroller, 300, 220);
    fireEvent.touchEnd(scroller, { touches: [] });
    fireEvent.click(scroller);
    expect(document.activeElement).not.toBe(input());
  });

  it("gears a short touch drag up to a notch", () => {
    // 14px is short of a 16.8px line; only the touch gain makes it a notch.
    const { scroller, wheel } = term();
    drag(scroller, 300, 286);
    expect(wheel).toHaveBeenCalledTimes(1);
  });

  it("reports touch wheels at pane 0's middle row and desktop wheels at the pointer row", () => {
    // Position-aware apps ignore wheels over their input box, so touch uses the middle row.
    const { scroller, wheel } = term();
    drag(scroller, 300, 266);
    expect(wheel.mock.calls[0]![3]).toBe(2);
    wheel.mockClear();
    fireEvent.wheel(scroller, { deltaY: 120, clientX: 100, clientY: 266 });
    expect(wheel.mock.calls[0]![3]).toBe(3);

    const split = term({ ...alt, rows: 8, pane0: { cols: 80, rows: 2 } });
    drag(split.scroller, 300, 266);
    expect(split.wheel.mock.calls[0]![3]).toBe(1);
  });

  it("does not enter reading mode on scroll while forwarding", () => {
    const { scroller, props } = term();
    fireEvent.scroll(scroller);
    expect(props.enterReading).not.toHaveBeenCalled();
  });

  it("relays text entered into the session-selection keyboard proxy", () => {
    const proxy = document.createElement("textarea");
    proxy.dataset.keyboardProxy = "";
    document.body.append(proxy);
    try {
      const { props } = term();
      deliverMobileKeyboardProxyInput({ inputType: "insertText", data: "hello", isComposing: false });
      expect(props.sendData).toHaveBeenCalledWith("hello");
    } finally {
      proxy.remove();
    }
  });
});

describe("MobileLiveTerminal mouse button forwarding", () => {
  it("forwards a press, a per-cell drag report, and a release", () => {
    // Exact per-cell dedupe needs real metrics (tests/live-click-forward.spec.ts); this pins the shape.
    const { scroller, button } = term();
    fireEvent.pointerDown(scroller, mouse());
    fireEvent.pointerMove(scroller, mouse({ clientY: 40 }));
    fireEvent.pointerUp(scroller, mouse({ clientY: 40 }));
    const calls = button.mock.calls;
    expect(calls[0]!.slice(0, 3)).toEqual([0, false, false]);
    expect(calls.some((c) => c[1] === false && c[2] === true)).toBe(true);
    expect(calls.at(-1)![1]).toBe(true);
  });

  it.each([
    ["a Shift+click, which keeps local selection", mouse({ shiftKey: true })],
    ["a touch pointer, which keeps its own path", mouse({ pointerType: "touch" })],
  ])("does not forward %s", (_n, init) => {
    const { scroller, button } = term();
    fireEvent.pointerDown(scroller, init);
    expect(button).not.toHaveBeenCalled();
  });

  it("maps split-window mouse input into pane 0", () => {
    const clamped = term({ ...alt, pane0: { cols: 1, rows: 1 } });
    fireEvent.pointerDown(clamped.scroller, mouse({ clientX: 500, clientY: 500 }));
    expect(clamped.button.mock.calls[0]!.slice(4)).toEqual([1, 1]);
    clamped.unmount();

    const rowAt = (top: number) => {
      const view = term({ ...alt, pane0: { cols: 80, rows: 24, left: 0, top } });
      fireEvent.pointerDown(view.scroller, mouse({ clientX: 100, clientY: 100 }));
      const row = view.button.mock.calls[0]![5] as number;
      view.unmount();
      return row;
    };
    expect(rowAt(1)).toBe(rowAt(0) - 1);
  });
});

describe("MobileLiveTerminal forward-mode flick momentum", () => {
  // Handlers read performance.now(), so faking it lets the tests drive release velocity.
  beforeEach(() => {
    vi.useFakeTimers({
      toFake: [
        "setTimeout",
        "clearTimeout",
        "setInterval",
        "clearInterval",
        "requestAnimationFrame",
        "cancelAnimationFrame",
        "performance",
      ],
    });
  });
  afterEach(() => vi.useRealTimers());

  /** 2 px/ms upward: four 32px moves 16ms apart. */
  function flick(scroller: HTMLElement) {
    fireEvent.touchStart(scroller, { touches: [touch(400)] });
    for (let y = 368; y >= 272; y -= 32) {
      vi.advanceTimersByTime(16);
      fireEvent.touchMove(scroller, { touches: [touch(y)] });
    }
    fireEvent.touchEnd(scroller, { touches: [] });
  }
  /** Asserts no notch is forwarded over the next second. */
  const expectStill = (wheel: Mock) => {
    const before = wheel.mock.calls.length;
    vi.advanceTimersByTime(1_000);
    expect(wheel.mock.calls.length).toBe(before);
  };

  it("coasts in the drag's direction and decays to a stop", () => {
    const { scroller, wheel } = term();
    flick(scroller);
    const atLift = wheel.mock.calls.length;
    vi.advanceTimersByTime(300);
    expect(wheel.mock.calls.length).toBeGreaterThan(atLift);
    expect(wheel.mock.calls.at(-1)![0]).toBe(false);
    vi.advanceTimersByTime(10_000);
    expectStill(wheel);
  });

  it("stops the coast on a new touch", () => {
    const { scroller, wheel } = term();
    flick(scroller);
    vi.advanceTimersByTime(100);
    fireEvent.touchStart(scroller, { touches: [touch(200)] });
    expectStill(wheel);
  });

  it("stops the coast when the user types, and the key still goes out", () => {
    const { scroller, wheel, input, props } = term();
    flick(scroller);
    vi.advanceTimersByTime(100);
    fireEvent.keyDown(input(), { key: "Enter" });
    expect(props.sendData).toHaveBeenCalledWith("\r");
    expectStill(wheel);
  });

  it.each([
    // Hold still past FLICK_MAX_PAUSE_MS before lifting.
    ["paused before the lift", 16, 32, 200],
    // 8px over 100ms is under FLICK_MIN_VELOCITY.
    ["was slow", 100, 8, 0],
  ])("does not coast when the drag %s", (_n, moveAfter, distance, holdMs) => {
    const { scroller, wheel } = term();
    fireEvent.touchStart(scroller, { touches: [touch(400)] });
    vi.advanceTimersByTime(moveAfter);
    fireEvent.touchMove(scroller, { touches: [touch(400 - distance)] });
    vi.advanceTimersByTime(holdMs);
    fireEvent.touchEnd(scroller, { touches: [] });
    expectStill(wheel);
  });
});

describe("MobileLiveTerminal link clicks in forward mode", () => {
  const linked = () => {
    const view = term({ ...alt, content: "see https://example.com/x\n" } as Partial<LiveFrame>);
    return { ...view, anchor: view.container.querySelector("a[href='https://example.com/x']") as HTMLElement };
  };

  it("lets a primary press on a link reach the anchor (#3918)", () => {
    const { anchor, button } = linked();
    // A real press lands on one of the anchor's cell spans.
    expect(fireEvent.pointerDown(anchor.querySelector("span")!, mouse())).toBe(true);
    fireEvent.pointerUp(anchor, mouse());
    expect(button).not.toHaveBeenCalled();
  });

  it("still forwards a press on plain output and a right-click on a link", () => {
    const { scroller, anchor, button } = linked();
    fireEvent.pointerDown(scroller, mouse());
    expect(button).toHaveBeenCalled();
    button.mockClear();
    fireEvent.pointerDown(anchor, mouse({ button: 2 }));
    expect(button.mock.calls[0]![0]).toBe(2);
  });
});

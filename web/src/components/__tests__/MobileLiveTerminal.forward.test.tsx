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
    expect(wheel).toHaveBeenCalled();
    // deltaY > 0 = scroll down = wheel down (up === false). The daemon picks
    // the encoding from the pane's own modes, so none is passed here.
    expect(wheel.mock.calls[0][0]).toBe(false);
    fireEvent.wheel(scroller, { deltaY: -120 });
    expect(wheel.mock.calls.at(-1)![0]).toBe(true);
    // A line-mode delta still forwards a notch.
    wheel.mockClear();
    fireEvent.wheel(scroller, { deltaY: 3, deltaMode: 1 });
    expect(wheel).toHaveBeenCalled();
  });

  // A full-screen app without mouse tracking still owns its own scrollback,
  // and the daemon sends it PageUp/PageDown. Scrolling the browser's spacer
  // of unrelated normal-buffer history would show the user nothing.
  it("forwards the wheel to a full-screen app that never enabled mouse tracking", () => {
    const { scroller, wheel, button } = term(frame({ altScreen: true, mouse: false }));
    expect(scroller.className).toContain("overflow-hidden");
    fireEvent.wheel(scroller, { deltaY: 120 });
    expect(wheel).toHaveBeenCalled();
    // A button report is the part that needs tracking: an app that never
    // asked for one reads it as typed escape bytes.
    fireEvent.pointerDown(scroller, { pointerType: "mouse", button: 0, clientX: 10, clientY: 20 });
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

  it("maps split-window mouse input into pane 0", () => {
    const clamped = term(frame({ altScreen: true, mouse: true, mouseSgr: true, pane0: { cols: 1, rows: 1 } }));
    fireEvent.pointerDown(clamped.scroller, { pointerType: "mouse", button: 0, clientX: 500, clientY: 500 });
    expect(clamped.button.mock.calls[0]!.slice(4)).toEqual([1, 1]);
    clamped.unmount();

    const base = term(
      frame({ altScreen: true, mouse: true, mouseSgr: true, pane0: { cols: 80, rows: 24, left: 0, top: 0 } }),
    );
    fireEvent.pointerDown(base.scroller, { pointerType: "mouse", button: 0, clientX: 100, clientY: 100 });
    const baseRow = base.button.mock.calls[0]![5] as number;
    base.unmount();

    const offset = term(
      frame({ altScreen: true, mouse: true, mouseSgr: true, pane0: { cols: 80, rows: 24, left: 0, top: 1 } }),
    );
    fireEvent.pointerDown(offset.scroller, { pointerType: "mouse", button: 0, clientX: 100, clientY: 100 });
    const offsetRow = offset.button.mock.calls[0]![5] as number;
    expect(offsetRow).toBe(baseRow - 1);
  });

  it("does NOT forward a click for a normal-screen agent", () => {
    const { scroller, button } = term(frame({ altScreen: false, mouse: true, mouseSgr: true }));
    fireEvent.pointerDown(scroller, { pointerType: "mouse", button: 0, clientX: 10, clientY: 10 });
    expect(button).not.toHaveBeenCalled();
  });

  it("does NOT forward a Shift+click (keeps local text selection)", () => {
    const { scroller, button } = term(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    fireEvent.pointerDown(scroller, { pointerType: "mouse", button: 0, shiftKey: true, clientX: 10, clientY: 10 });
    expect(button).not.toHaveBeenCalled();
  });

  it("does NOT forward a touch pointer (touch keeps its own scroll path)", () => {
    const { scroller, button } = term(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    fireEvent.pointerDown(scroller, { pointerType: "touch", button: 0, clientX: 10, clientY: 10 });
    expect(button).not.toHaveBeenCalled();
  });

  it("forwards a drag motion report and finalizes on release", () => {
    // Exact per-cell dedupe counts depend on measured char metrics, which are
    // unstable in jsdom; that is asserted in the real browser by
    // tests/live-click-forward.spec.ts. Here we just lock the gesture shape:
    // press (no motion) -> drag (motion bit) -> release. The drag moves in Y
    // (row space): columns clamp to 1 in jsdom because renderCols never
    // settles, so a horizontal move would dedupe to the same cell.
    const { scroller, button } = term(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    fireEvent.pointerDown(scroller, { pointerType: "mouse", button: 0, clientX: 10, clientY: 10 });
    fireEvent.pointerMove(scroller, { pointerType: "mouse", clientX: 10, clientY: 40 });
    fireEvent.pointerUp(scroller, { pointerType: "mouse", button: 0, clientX: 10, clientY: 40 });
    const calls = button.mock.calls;
    expect(calls[0]!.slice(1, 3)).toEqual([false, false]); // press: not release, not motion
    expect(calls.some((c) => c[1] === false && c[2] === true)).toBe(true); // a drag (motion) report
    expect(calls.at(-1)![1]).toBe(true); // release last
  });

  it("gears a touch drag up by the forward touch gain", () => {
    const { scroller, wheel } = term(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    // lineH = 14 * 1.2 = 16.8px, so 14px of finger travel is short of a line
    // and reaches one notch only because of the assist; ungeared it would
    // round to nothing. A drag this small fits in one burst and leaves at
    // once (pacing across bursts is covered in the alt-screen spec).
    fireEvent.touchStart(scroller, { touches: [{ clientX: 100, clientY: 300 } as Touch] });
    fireEvent.touchMove(scroller, { touches: [{ clientX: 100, clientY: 286 } as Touch] });
    expect(wheel).toHaveBeenCalledTimes(1);
  });

  it("reports touch wheels at the input pane's middle row; desktop wheels keep the pointer cell", () => {
    // Position-aware apps (Claude Code) hit-test the wheel's row and ignore
    // notches over their pinned input box, which shrank the usable touch area
    // to the transcript sliver above it. The touch path therefore clamps to
    // the pane's vertical middle (rows=3 -> row 2) no matter where the finger
    // is; the desktop pointer keeps real hover semantics (y=266 -> row 3).
    const { scroller, wheel } = term(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    fireEvent.touchStart(scroller, { touches: [{ clientX: 100, clientY: 300 } as Touch] });
    fireEvent.touchMove(scroller, { touches: [{ clientX: 100, clientY: 266 } as Touch] });
    expect(wheel.mock.calls[0]![2]).toBe(2);
    wheel.mockClear();
    fireEvent.wheel(scroller, { deltaY: 120, clientX: 100, clientY: 266 });
    expect(wheel.mock.calls[0]![2]).toBe(3);

    // A top/bottom split retains the composite's full row count in `rows`,
    // but touch input stays in pane 0 and must use that pane's smaller extent.
    const split = term(frame({ rows: 8, altScreen: true, mouse: true, mouseSgr: true, pane0: { cols: 80, rows: 2 } }));
    fireEvent.touchStart(split.scroller, { touches: [{ clientX: 100, clientY: 300 } as Touch] });
    fireEvent.touchMove(split.scroller, { touches: [{ clientX: 100, clientY: 266 } as Touch] });
    expect(split.wheel.mock.calls[0]![2]).toBe(1);
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

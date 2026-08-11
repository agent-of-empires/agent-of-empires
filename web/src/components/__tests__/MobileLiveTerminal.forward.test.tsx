// @vitest-environment jsdom
//
// The mobile live view forwards the wheel to a full-screen mouse app
// (alternate screen) instead of scrolling the useless normal-buffer
// capture. This guards that routing: forward only when the frame reports
// altScreen && mouse, and not otherwise. Byte encodings are covered by
// ../../lib/__tests__/liveMouse.test.ts.

import { createRef } from "react";
import { describe, expect, it, vi, beforeAll } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { MobileLiveTerminal } from "../MobileLiveTerminal";
import type { LiveFrame } from "../../hooks/useLiveTerminal";
import { deliverMobileKeyboardProxyInput } from "../../lib/mobileKeyboardProxy";

// Both font keys: jsdom's matchMedia reports a fine pointer, so the component
// reads desktopFontSize; leaving it undefined made fontSize (and every px of
// grid math) NaN, which the cell-coordinate assertions below would trip over.
vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));

beforeAll(() => {
  // The component observes its container; jsdom has no ResizeObserver.
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;
});

function frame(over: Partial<LiveFrame>): LiveFrame {
  return {
    content: "a\nb\nc\n",
    rows: 3,
    history: 1000,
    cursor: null,
    altScreen: false,
    mouse: false,
    mouseSgr: false,
    pane0: null,
    ...over,
  };
}

function renderTerm(f: LiveFrame, forwardWheel = vi.fn(), forwardButton = vi.fn(), sendData = vi.fn()) {
  const inputRef = createRef<HTMLTextAreaElement>();
  const utils = render(
    <MobileLiveTerminal
      frame={f}
      connected
      active
      reading={false}
      sendResize={vi.fn()}
      setWindow={vi.fn()}
      setCadence={vi.fn()}
      enterReading={vi.fn()}
      returnToLive={vi.fn()}
      sendData={sendData}
      forwardWheel={forwardWheel}
      forwardButton={forwardButton}
      ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
      clearCtrl={vi.fn()}
      inputRef={inputRef}
      onInputFocusChange={vi.fn()}
      bottomAlign
      keyboardOpen={false}
    />,
  );
  const scroller = utils.container.querySelector("[data-live-terminal] > div") as HTMLElement;
  return { ...utils, scroller, forwardWheel, forwardButton, sendData, inputRef };
}

describe("MobileLiveTerminal wheel forwarding", () => {
  it("forwards the wheel to a full-screen mouse app and pins the live edge", () => {
    const { scroller, forwardWheel } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    expect(scroller.className).toContain("overflow-hidden");
    fireEvent.wheel(scroller, { deltaY: 120 });
    expect(forwardWheel).toHaveBeenCalled();
    // deltaY > 0 = scroll down = wheel down (up === false), SGR encoding.
    expect(forwardWheel.mock.calls[0][0]).toBe(false);
    expect(forwardWheel.mock.calls[0][1]).toBe(true);
    fireEvent.wheel(scroller, { deltaY: -120 });
    const lastUp = forwardWheel.mock.calls[forwardWheel.mock.calls.length - 1][0];
    expect(lastUp).toBe(true);
  });

  it("declares touch-action none in forward mode so the page cannot pan", () => {
    // React's delegated touch listeners are passive, so preventDefault in
    // onTouchMove cannot stop the browser's native pan; touch-action: none
    // on the scroller is what keeps a drag from scrolling the whole page
    // (worst with the soft keyboard open) while the wheel forwards.
    const { scroller } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    expect(scroller.style.touchAction).toBe("none");
  });

  it("cancels a native touch move in forward mode as a WebKit fallback", () => {
    const { scroller } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    const move = new Event("touchmove", { cancelable: true });
    scroller.dispatchEvent(move);
    expect(move.defaultPrevented).toBe(true);
  });

  it("leaves touch-action unset outside forward mode (native capture scroll)", () => {
    const { scroller } = renderTerm(frame({ altScreen: false, mouse: false }));
    expect(scroller.style.touchAction).toBe("");
    const move = new Event("touchmove", { cancelable: true });
    scroller.dispatchEvent(move);
    expect(move.defaultPrevented).toBe(false);
  });

  it("normalizes line-mode wheel deltas (deltaMode 1)", () => {
    const { scroller, forwardWheel } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    // deltaMode 1 = lines; a few lines should still forward at least one notch.
    fireEvent.wheel(scroller, { deltaY: 3, deltaMode: 1 });
    expect(forwardWheel).toHaveBeenCalled();
  });

  it("does NOT forward when the app has no mouse mode (keeps capture scroll)", () => {
    const { scroller, forwardWheel } = renderTerm(frame({ altScreen: true, mouse: false }));
    expect(scroller.className).toContain("overflow-y-auto");
    fireEvent.wheel(scroller, { deltaY: 120 });
    expect(forwardWheel).not.toHaveBeenCalled();
  });

  it("does NOT forward for a normal-screen agent", () => {
    const { scroller, forwardWheel } = renderTerm(frame({ altScreen: false, mouse: true, mouseSgr: true }));
    fireEvent.wheel(scroller, { deltaY: 120 });
    expect(forwardWheel).not.toHaveBeenCalled();
  });

  it("forwards a single-finger drag as wheel notches", () => {
    const { scroller, forwardWheel } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    const touch = (y: number) => ({ clientX: 100, clientY: y }) as Touch;
    // Finger moves UP (y decreases) => content scrolls down => wheel down.
    fireEvent.touchStart(scroller, { touches: [touch(300)] });
    fireEvent.touchMove(scroller, { touches: [touch(220)] });
    fireEvent.touchEnd(scroller, { touches: [] });
    expect(forwardWheel).toHaveBeenCalled();
    expect(forwardWheel.mock.calls[0][0]).toBe(false); // up === false (wheel down)
  });

  it("uses touchend's final position when iOS coalesces a quick swipe", () => {
    const { scroller, forwardWheel } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    fireEvent.touchStart(scroller, { touches: [{ clientX: 100, clientY: 300 } as Touch] });
    fireEvent.touchEnd(scroller, {
      touches: [],
      changedTouches: [{ clientX: 100, clientY: 220 } as Touch],
    });
    expect(forwardWheel).toHaveBeenCalled();
    expect(forwardWheel.mock.calls[0][0]).toBe(false);
  });

  it("does not turn a forward-mode swipe into a keyboard-opening click", () => {
    const { scroller, inputRef } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    const touch = (y: number) => ({ clientX: 100, clientY: y }) as Touch;
    fireEvent.touchStart(scroller, { touches: [touch(300)] });
    fireEvent.touchMove(scroller, { touches: [touch(220)] });
    fireEvent.touchEnd(scroller, { touches: [] });
    // WebKit can synthesize this click even though touch-action:none kept the
    // browser from recognizing a native scroll. A moved touch must not focus
    // the hidden terminal input and raise the keyboard.
    fireEvent.click(scroller);
    expect(document.activeElement).not.toBe(inputRef.current);
  });

  it("forwards a mouse click (press then release) to a full-screen mouse app", () => {
    const { scroller, forwardButton } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    fireEvent.pointerDown(scroller, { pointerType: "mouse", button: 0, clientX: 10, clientY: 10 });
    fireEvent.pointerUp(scroller, { pointerType: "mouse", button: 0, clientX: 10, clientY: 10 });
    expect(forwardButton).toHaveBeenCalledTimes(2);
    // press: base left=0, release=false; then release=true.
    expect(forwardButton.mock.calls[0].slice(0, 3)).toEqual([0, false, false]);
    expect(forwardButton.mock.calls[1][1]).toBe(true);
  });

  it("clamps split-window mouse input to pane 0", () => {
    const { scroller, forwardButton } = renderTerm(
      frame({ altScreen: true, mouse: true, mouseSgr: true, pane0: { cols: 1, rows: 1 } }),
    );
    fireEvent.pointerDown(scroller, { pointerType: "mouse", button: 0, clientX: 500, clientY: 500 });
    expect(forwardButton.mock.calls[0]!.slice(4)).toEqual([1, 1]);
  });

  it("does NOT forward a click for a normal-screen agent", () => {
    const { scroller, forwardButton } = renderTerm(frame({ altScreen: false, mouse: true, mouseSgr: true }));
    fireEvent.pointerDown(scroller, { pointerType: "mouse", button: 0, clientX: 10, clientY: 10 });
    expect(forwardButton).not.toHaveBeenCalled();
  });

  it("does NOT forward a Shift+click (keeps local text selection)", () => {
    const { scroller, forwardButton } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    fireEvent.pointerDown(scroller, { pointerType: "mouse", button: 0, shiftKey: true, clientX: 10, clientY: 10 });
    expect(forwardButton).not.toHaveBeenCalled();
  });

  it("does NOT forward a touch pointer (touch keeps its own scroll path)", () => {
    const { scroller, forwardButton } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    fireEvent.pointerDown(scroller, { pointerType: "touch", button: 0, clientX: 10, clientY: 10 });
    expect(forwardButton).not.toHaveBeenCalled();
  });

  it("forwards a drag motion report and finalizes on release", () => {
    // Exact per-cell dedupe counts depend on measured char metrics, which are
    // unstable in jsdom; that is asserted in the real browser by
    // tests/live-click-forward.spec.ts. Here we just lock the gesture shape:
    // press (no motion) -> drag (motion bit) -> release. The drag moves in Y
    // (row space): columns clamp to 1 in jsdom because renderCols never
    // settles, so a horizontal move would dedupe to the same cell.
    const { scroller, forwardButton } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    fireEvent.pointerDown(scroller, { pointerType: "mouse", button: 0, clientX: 10, clientY: 10 });
    fireEvent.pointerMove(scroller, { pointerType: "mouse", clientX: 10, clientY: 40 });
    fireEvent.pointerUp(scroller, { pointerType: "mouse", button: 0, clientX: 10, clientY: 40 });
    const calls = forwardButton.mock.calls;
    expect(calls[0]!.slice(1, 3)).toEqual([false, false]); // press: not release, not motion
    expect(calls.some((c) => c[1] === false && c[2] === true)).toBe(true); // a drag (motion) report
    expect(calls.at(-1)![1]).toBe(true); // release last
  });

  it("gears a touch drag up by the forward touch gain", () => {
    const { scroller, forwardWheel } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    // lineH = 14 * 1.2 = 16.8px: a 34px drag produces two notches with the
    // small assist, but only the first leaves immediately. The second drains
    // on the next animation frame to avoid a delayed remote redraw jumping.
    fireEvent.touchStart(scroller, { touches: [{ clientX: 100, clientY: 300 } as Touch] });
    fireEvent.touchMove(scroller, { touches: [{ clientX: 100, clientY: 266 } as Touch] });
    expect(forwardWheel).toHaveBeenCalledTimes(1);
  });

  it("reports touch wheels at the input pane's middle row; desktop wheels keep the pointer cell", () => {
    // Position-aware apps (Claude Code) hit-test the wheel's row and ignore
    // notches over their pinned input box, which shrank the usable touch area
    // to the transcript sliver above it. The touch path therefore clamps to
    // the pane's vertical middle (rows=3 -> row 2) no matter where the finger
    // is; the desktop pointer keeps real hover semantics (y=266 -> row 3).
    const { scroller, forwardWheel } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    fireEvent.touchStart(scroller, { touches: [{ clientX: 100, clientY: 300 } as Touch] });
    fireEvent.touchMove(scroller, { touches: [{ clientX: 100, clientY: 266 } as Touch] });
    expect(forwardWheel.mock.calls[0]![3]).toBe(2);
    forwardWheel.mockClear();
    fireEvent.wheel(scroller, { deltaY: 120, clientX: 100, clientY: 266 });
    expect(forwardWheel.mock.calls[0]![3]).toBe(3);

    // A top/bottom split retains the composite's full row count in `rows`,
    // but touch input stays in pane 0 and must use that pane's smaller extent.
    const split = renderTerm(
      frame({ rows: 8, altScreen: true, mouse: true, mouseSgr: true, pane0: { cols: 80, rows: 2 } }),
    );
    fireEvent.touchStart(split.scroller, { touches: [{ clientX: 100, clientY: 300 } as Touch] });
    fireEvent.touchMove(split.scroller, { touches: [{ clientX: 100, clientY: 266 } as Touch] });
    expect(split.forwardWheel.mock.calls[0]![3]).toBe(1);
  });

  it("does not enter reading mode on scroll while forwarding", () => {
    const enterReading = vi.fn();
    const utils = render(
      <MobileLiveTerminal
        frame={frame({ altScreen: true, mouse: true, mouseSgr: true })}
        connected
        active
        reading={false}
        sendResize={vi.fn()}
        setWindow={vi.fn()}
        setCadence={vi.fn()}
        enterReading={enterReading}
        returnToLive={vi.fn()}
        sendData={vi.fn()}
        forwardWheel={vi.fn()}
        forwardButton={vi.fn()}
        ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
        clearCtrl={vi.fn()}
        inputRef={createRef<HTMLTextAreaElement>()}
        onInputFocusChange={vi.fn()}
        bottomAlign
        keyboardOpen={false}
      />,
    );
    const scroller = utils.container.querySelector("[data-live-terminal] > div") as HTMLElement;
    fireEvent.scroll(scroller);
    expect(enterReading).not.toHaveBeenCalled();
  });

  it("relays text entered into the session-selection keyboard proxy", () => {
    const proxy = document.createElement("textarea");
    proxy.dataset.keyboardProxy = "";
    document.body.append(proxy);
    try {
      const { sendData } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
      deliverMobileKeyboardProxyInput({
        inputType: "insertText",
        data: "hello",
        isComposing: false,
      });
      expect(sendData).toHaveBeenCalledWith("hello");
    } finally {
      proxy.remove();
    }
  });
});

describe("MobileLiveTerminal forward-mode flick momentum", () => {
  // Forward mode has no native scroller (overflow hidden), so flick inertia
  // is synthesized: sampled release velocity, decaying rAF coast through the
  // wheel-forward path. Fake rAF + performance so the tests control time; the
  // handlers read performance.now(), not event timestamps, for this reason.
  const FAKED = [
    "setTimeout",
    "clearTimeout",
    "setInterval",
    "clearInterval",
    "requestAnimationFrame",
    "cancelAnimationFrame",
    "performance",
  ] as const;
  const tp = (y: number) => ({ clientX: 100, clientY: y }) as Touch;

  function flick(scroller: HTMLElement) {
    // 2 px/ms upward: four 32px moves at 16ms apart.
    fireEvent.touchStart(scroller, { touches: [tp(400)] });
    let y = 400;
    for (let i = 0; i < 4; i++) {
      vi.advanceTimersByTime(16);
      y -= 32;
      fireEvent.touchMove(scroller, { touches: [tp(y)] });
    }
    fireEvent.touchEnd(scroller, { touches: [] });
  }

  it("coasts with decaying momentum after a flick, in the drag's direction", () => {
    vi.useFakeTimers({ toFake: [...FAKED] });
    try {
      const { scroller, forwardWheel } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
      flick(scroller);
      const atLift = forwardWheel.mock.calls.length;
      vi.advanceTimersByTime(300);
      const coasted = forwardWheel.mock.calls.length;
      expect(coasted).toBeGreaterThan(atLift);
      // Finger up = wheel down, during the drag AND the coast.
      expect(forwardWheel.mock.calls[coasted - 1]![0]).toBe(false);
      // The decay must actually end the coast rather than scrolling forever.
      vi.advanceTimersByTime(10_000);
      const settled = forwardWheel.mock.calls.length;
      vi.advanceTimersByTime(1_000);
      expect(forwardWheel.mock.calls.length).toBe(settled);
    } finally {
      vi.useRealTimers();
    }
  });

  it("stops the coast the moment a new touch lands", () => {
    vi.useFakeTimers({ toFake: [...FAKED] });
    try {
      const { scroller, forwardWheel } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
      flick(scroller);
      vi.advanceTimersByTime(100);
      const beforeStop = forwardWheel.mock.calls.length;
      fireEvent.touchStart(scroller, { touches: [tp(200)] });
      vi.advanceTimersByTime(1_000);
      expect(forwardWheel.mock.calls.length).toBe(beforeStop);
    } finally {
      vi.useRealTimers();
    }
  });

  it("stops the coast when the user types", () => {
    vi.useFakeTimers({ toFake: [...FAKED] });
    try {
      const { container, scroller, forwardWheel, sendData } = renderTerm(
        frame({ altScreen: true, mouse: true, mouseSgr: true }),
      );
      flick(scroller);
      vi.advanceTimersByTime(100);
      const beforeType = forwardWheel.mock.calls.length;
      // A keystroke on the hidden input mid-coast: the key must go out AND the
      // wheel storm must end, so the app echoes it instead of scrolling.
      const input = container.querySelector("textarea") as HTMLTextAreaElement;
      fireEvent.keyDown(input, { key: "Enter" });
      expect(sendData).toHaveBeenCalledWith("\r");
      vi.advanceTimersByTime(1_000);
      expect(forwardWheel.mock.calls.length).toBe(beforeType);
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not coast when the drag paused before the lift", () => {
    vi.useFakeTimers({ toFake: [...FAKED] });
    try {
      const { scroller, forwardWheel } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
      fireEvent.touchStart(scroller, { touches: [tp(400)] });
      vi.advanceTimersByTime(16);
      fireEvent.touchMove(scroller, { touches: [tp(368)] });
      // Hold still past FLICK_MAX_PAUSE_MS, then lift.
      vi.advanceTimersByTime(200);
      fireEvent.touchEnd(scroller, { touches: [] });
      const atLift = forwardWheel.mock.calls.length;
      vi.advanceTimersByTime(1_000);
      expect(forwardWheel.mock.calls.length).toBe(atLift);
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not coast after a slow drag", () => {
    vi.useFakeTimers({ toFake: [...FAKED] });
    try {
      const { scroller, forwardWheel } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
      fireEvent.touchStart(scroller, { touches: [tp(400)] });
      // 8px over 100ms = 0.08 px/ms, well under FLICK_MIN_VELOCITY.
      vi.advanceTimersByTime(100);
      fireEvent.touchMove(scroller, { touches: [tp(392)] });
      fireEvent.touchEnd(scroller, { touches: [] });
      const atLift = forwardWheel.mock.calls.length;
      vi.advanceTimersByTime(1_000);
      expect(forwardWheel.mock.calls.length).toBe(atLift);
    } finally {
      vi.useRealTimers();
    }
  });
});

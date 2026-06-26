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

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14 }, update: vi.fn() }),
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
    ...over,
  };
}

function renderTerm(f: LiveFrame, forwardWheel = vi.fn(), forwardButton = vi.fn()) {
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
      sendData={vi.fn()}
      forwardWheel={forwardWheel}
      forwardButton={forwardButton}
      ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
      clearCtrl={vi.fn()}
      inputRef={createRef<HTMLTextAreaElement>()}
      onInputFocusChange={vi.fn()}
      bottomAlign
    />,
  );
  const scroller = utils.container.querySelector("[data-live-terminal] > div") as HTMLElement;
  return { ...utils, scroller, forwardWheel, forwardButton };
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

  it("forwards a mouse click (press then release) to a full-screen mouse app", () => {
    const { scroller, forwardButton } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    fireEvent.pointerDown(scroller, { pointerType: "mouse", button: 0, clientX: 10, clientY: 10 });
    fireEvent.pointerUp(scroller, { pointerType: "mouse", button: 0, clientX: 10, clientY: 10 });
    expect(forwardButton).toHaveBeenCalledTimes(2);
    // press: base left=0, release=false; then release=true.
    expect(forwardButton.mock.calls[0].slice(0, 3)).toEqual([0, false, false]);
    expect(forwardButton.mock.calls[1][1]).toBe(true);
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
    // press (no motion) -> drag (motion bit) -> release.
    const { scroller, forwardButton } = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    fireEvent.pointerDown(scroller, { pointerType: "mouse", button: 0, clientX: 10, clientY: 10 });
    fireEvent.pointerMove(scroller, { pointerType: "mouse", clientX: 120, clientY: 10 });
    fireEvent.pointerUp(scroller, { pointerType: "mouse", button: 0, clientX: 120, clientY: 10 });
    const calls = forwardButton.mock.calls;
    expect(calls[0]!.slice(1, 3)).toEqual([false, false]); // press: not release, not motion
    expect(calls.some((c) => c[1] === false && c[2] === true)).toBe(true); // a drag (motion) report
    expect(calls.at(-1)![1]).toBe(true); // release last
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
      />,
    );
    const scroller = utils.container.querySelector("[data-live-terminal] > div") as HTMLElement;
    fireEvent.scroll(scroller);
    expect(enterReading).not.toHaveBeenCalled();
  });
});

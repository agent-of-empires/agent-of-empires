// @vitest-environment jsdom
//
// Alt-screen behavior: a full-screen app owns its whole grid, so every row
// renders (no trailing-blank trimming), and forwarded touch notches are paced
// to the app's redraws (one out, the next when a frame arrives or the ack
// timeout passes) rather than released on a fixed animation-frame clock.

import { createRef } from "react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render } from "@testing-library/react";
import { MobileLiveTerminal } from "../MobileLiveTerminal";
import type { LiveFrame } from "../../hooks/useLiveTerminal";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));

beforeAll(() => {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;
});

function frame(over: Partial<LiveFrame>): LiveFrame {
  return {
    content: "a\nb\nc\n\n\n",
    rows: 5,
    history: 0,
    cursor: null,
    altScreen: false,
    mouse: false,
    mouseSgr: false,
    pane0: null,
    ...over,
  };
}

function renderTerm(f: LiveFrame, forwardWheel = vi.fn()) {
  const inputRef = createRef<HTMLTextAreaElement>();
  const view = (current: LiveFrame) => (
    <MobileLiveTerminal
      frame={current}
      connected
      active
      reading={false}
      sendResize={vi.fn()}
      setWindow={vi.fn()}
      setCadence={vi.fn()}
      enterReading={vi.fn()}
      returnToLive={vi.fn()}
      sendData={vi.fn()}
      uploadPastedImage={vi.fn(async () => null)}
      forwardWheel={forwardWheel}
      forwardButton={vi.fn()}
      ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
      clearCtrl={vi.fn()}
      inputRef={inputRef}
      onInputFocusChange={vi.fn()}
      bottomAlign
      keyboardOpen={false}
    />
  );
  const utils = render(view(f));
  const scroller = utils.container.querySelector("[data-live-terminal] > div") as HTMLElement;
  const rows = () => utils.container.querySelectorAll("[data-live-content] > div:not([aria-hidden])").length;
  /** Deliver a new frame: the app's acknowledgement of forwarded input. */
  const showFrame = (next: LiveFrame) => utils.rerender(view(next));
  return { ...utils, scroller, rows, showFrame, forwardWheel };
}

describe("MobileLiveTerminal on the alternate screen", () => {
  it("renders every grid row instead of trimming trailing blanks", () => {
    const normal = renderTerm(frame({ altScreen: false }));
    expect(normal.rows()).toBe(3);
    normal.unmount();
    const alt = renderTerm(frame({ altScreen: true, mouse: true, mouseSgr: true }));
    expect(alt.rows()).toBe(5);
  });

  describe("notch pacing", () => {
    beforeEach(() => vi.useFakeTimers());
    afterEach(() => vi.useRealTimers());

    it("moves a slow drag a line at a time, with no wait between lines", () => {
      const f = frame({ altScreen: true, mouse: true, mouseSgr: true, lines: ["a", "b", "c", "", ""] });
      const { scroller, forwardWheel } = renderTerm(f);
      const touch = (y: number) => [{ clientX: 40, clientY: y, identifier: 1, target: scroller }];
      // Each step earns a single line (14px of travel over a 16.8px line,
      // geared by the assist), which is what a deliberate drag looks like.
      fireEvent.touchStart(scroller, { touches: touch(300) });
      fireEvent.touchMove(scroller, { touches: touch(286) });
      expect(forwardWheel).toHaveBeenCalledTimes(1);
      // No timer is advanced here: an emptied queue leaves nothing pending,
      // so the next line the finger earns goes out at once rather than
      // waiting out a pacing gap.
      fireEvent.touchMove(scroller, { touches: touch(272) });
      expect(forwardWheel).toHaveBeenCalledTimes(2);
    });

    it("clears a fast drag in larger steps, and loses none of it", () => {
      const f = frame({ altScreen: true, mouse: true, mouseSgr: true, lines: ["a", "b", "c", "", ""] });
      const { scroller, forwardWheel, showFrame } = renderTerm(f);
      const touch = (y: number) => [{ clientX: 40, clientY: y, identifier: 1, target: scroller }];
      // 200px of travel asks for 14 lines at once: the finger has outrun the
      // queue, so the release is sized to the backlog instead of one line.
      fireEvent.touchStart(scroller, { touches: touch(200) });
      fireEvent.touchMove(scroller, { touches: touch(400) });
      // Dragging down reveals older content, so the wheel goes up.
      expect(forwardWheel).toHaveBeenCalledTimes(4);
      expect(forwardWheel.mock.calls.every((call) => call[0] === true)).toBe(true);

      // A frame is the app's acknowledgement and releases the next step
      // without waiting out the fallback gap.
      showFrame({ ...f, lines: ["A", "b", "c", "", ""], content: "A\nb\nc\n\n\n" });
      expect(forwardWheel).toHaveBeenCalledTimes(7);

      // The rest drains on the fallback gap, in steps that shrink with the
      // backlog, and the whole gesture lands: 14 lines asked for, 14 sent.
      act(() => {
        vi.advanceTimersByTime(200);
      });
      expect(forwardWheel).toHaveBeenCalledTimes(14);
      act(() => {
        vi.advanceTimersByTime(500);
      });
      expect(forwardWheel).toHaveBeenCalledTimes(14);
    });
  });
});

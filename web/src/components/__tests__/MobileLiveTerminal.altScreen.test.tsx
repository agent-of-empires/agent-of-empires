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
      uploadPastedImage={vi.fn(async () => null)}
      forwardWheel={forwardWheel}
      forwardButton={vi.fn()}
      ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
      clearCtrl={vi.fn()}
      inputRef={inputRef}
      onInputFocusChange={vi.fn()}
      bottomAlign
      keyboardOpen={false}
    />,
  );
  const scroller = utils.container.querySelector("[data-live-terminal] > div") as HTMLElement;
  const rows = () => utils.container.querySelectorAll("[data-live-content] > div:not([aria-hidden])").length;
  return { ...utils, scroller, rows, forwardWheel };
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

    it("releases one notch per frame acknowledgement, or per ack timeout", () => {
      const f = frame({ altScreen: true, mouse: true, mouseSgr: true, lines: ["a", "b", "c", "", ""] });
      const { scroller, forwardWheel, rerender } = renderTerm(f);
      const touch = (y: number) => [{ clientX: 40, clientY: y, identifier: 1, target: scroller }];
      // A drag of three line-heights (lineH = 14 * 1.2 = 16.8px; gain 1.25).
      fireEvent.touchStart(scroller, { touches: touch(200) });
      fireEvent.touchMove(scroller, { touches: touch(200 + 45) });
      expect(forwardWheel).toHaveBeenCalledTimes(1);

      // No frame yet: the next notch waits for the ack timeout.
      act(() => {
        vi.advanceTimersByTime(30);
      });
      expect(forwardWheel).toHaveBeenCalledTimes(1);

      // A new frame is the app's acknowledgement and releases a notch.
      rerender(
        <MobileLiveTerminal
          frame={{ ...f, lines: ["A", "b", "c", "", ""], content: "A\nb\nc\n\n\n" }}
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
          inputRef={createRef<HTMLTextAreaElement>()}
          onInputFocusChange={vi.fn()}
          bottomAlign
          keyboardOpen={false}
        />,
      );
      expect(forwardWheel).toHaveBeenCalledTimes(2);

      // Without a frame, the timeout releases the remaining notch.
      act(() => {
        vi.advanceTimersByTime(60);
      });
      expect(forwardWheel).toHaveBeenCalledTimes(3);
      act(() => {
        vi.advanceTimersByTime(200);
      });
      expect(forwardWheel).toHaveBeenCalledTimes(3);
    });
  });
});

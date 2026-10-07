// @vitest-environment jsdom
// A full-screen app owns its whole grid, and forwarded touch notches are paced to its redraws.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent } from "@testing-library/react";
import { alt, installResizeObserver, liveFrame, renderLiveTerminal } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));
installResizeObserver();

const frame = (over = {}) => liveFrame({ content: "a\nb\nc\n\n\n", rows: 5, history: 0, ...over });

describe("MobileLiveTerminal on the alternate screen", () => {
  it("renders every grid row instead of trimming trailing blanks", () => {
    const normal = renderLiveTerminal({ frame: frame() });
    expect(normal.rowCount()).toBe(3);
    normal.unmount();
    expect(renderLiveTerminal({ frame: frame(alt) }).rowCount()).toBe(5);
  });

  describe("notch pacing", () => {
    beforeEach(() => vi.useFakeTimers());
    afterEach(() => vi.useRealTimers());

    function drag() {
      const f = frame({ ...alt, lines: ["a", "b", "c", "", ""] });
      const view = renderLiveTerminal({ frame: f });
      const touches = (y: number) => ({ touches: [{ clientX: 40, clientY: y, identifier: 1, target: view.scroller }] });
      return { ...view, f, wheel: view.props.forwardWheel as ReturnType<typeof vi.fn>, touches };
    }

    it("moves a slow drag a line at a time, with no wait between lines", () => {
      const { scroller, wheel, touches } = drag();
      // 14px over a 16.8px line earns one notch with the touch gain.
      fireEvent.touchStart(scroller, touches(300));
      fireEvent.touchMove(scroller, touches(286));
      expect(wheel).toHaveBeenCalledTimes(1);
      // An emptied queue leaves nothing pending, so the next line goes out at once.
      fireEvent.touchMove(scroller, touches(272));
      expect(wheel).toHaveBeenCalledTimes(2);
    });

    it("clears a fast drag in larger steps, and loses none of it", () => {
      const { scroller, wheel, touches, f, rerenderWith } = drag();
      // 200px asks for 14 lines; the burst is sized to the backlog.
      fireEvent.touchStart(scroller, touches(200));
      fireEvent.touchMove(scroller, touches(400));
      expect(wheel).toHaveBeenCalledTimes(4);
      expect(wheel.mock.calls.every((call) => call[0] === true)).toBe(true);
      // A frame acknowledges the burst and releases the next without waiting.
      rerenderWith({ frame: { ...f, lines: ["A", "b", "c", "", ""], content: "A\nb\nc\n\n\n" } });
      expect(wheel).toHaveBeenCalledTimes(7);
      act(() => vi.advanceTimersByTime(200));
      expect(wheel).toHaveBeenCalledTimes(14);
      act(() => vi.advanceTimersByTime(500));
      expect(wheel).toHaveBeenCalledTimes(14);
    });
  });
});

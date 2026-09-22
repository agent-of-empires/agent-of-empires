// @vitest-environment jsdom
// A full-screen app owns its whole grid, and forwarded touch notches are paced to its redraws.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent } from "@testing-library/react";
import { alt, installResizeObserver, liveFrame, renderLiveTerminal } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));
installResizeObserver();

/// Notches the gesture actually delivered. One message stands for a whole
/// release, so a call count would say how often the pacer fired, not how far
/// the pane was scrolled.
const notchesSent = (forwardWheel: ReturnType<typeof vi.fn>) =>
  forwardWheel.mock.calls.reduce((total: number, call) => total + ((call[3] as number) ?? 1), 0);

describe("MobileLiveTerminal on the alternate screen", () => {
  it("renders every grid row instead of trimming trailing blanks", () => {
    // Three lines of content and two trailing blanks in a five-row grid.
    const grid = { content: "a\nb\nc\n\n\n", rows: 5, history: 0 };
    const normal = renderLiveTerminal({ frame: liveFrame(grid) });
    expect(normal.rowCount()).toBe(3);
    normal.unmount();
    expect(renderLiveTerminal({ frame: liveFrame({ ...grid, ...alt }) }).rowCount()).toBe(5);
  });

  describe("notch pacing", () => {
    beforeEach(() => vi.useFakeTimers());
    afterEach(() => vi.useRealTimers());

    function drag() {
      const f = liveFrame({ ...alt, lines: ["a", "b", "c", "", ""] });
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
      const f = liveFrame({ ...alt, lines: ["a", "b", "c", "", ""] });
      const view = renderLiveTerminal({ frame: f });
      const { scroller } = view;
      const forwardWheel = view.props.forwardWheel as ReturnType<typeof vi.fn>;
      const showFrame = (next: LiveFrame) => view.rerenderWith({ frame: next });
      const touch = (y: number) => [{ clientX: 40, clientY: y, identifier: 1, target: scroller }];
      // 200px of travel asks for 14 lines at once: the finger has outrun the
      // queue, so the release is sized to the backlog instead of one line.
      fireEvent.touchStart(scroller, { touches: touch(200) });
      fireEvent.touchMove(scroller, { touches: touch(400) });
      // Dragging down reveals older content, so the wheel goes up.
      expect(notchesSent(forwardWheel)).toBe(4);
      expect(forwardWheel.mock.calls.every((call) => call[0] === true)).toBe(true);

      // A frame is the app's acknowledgement and releases the next step
      // without waiting out the fallback gap.
      showFrame({ ...f, lines: ["A", "b", "c", "", ""], content: "A\nb\nc\n\n\n" });
      expect(notchesSent(forwardWheel)).toBe(7);

      // The rest drains on the fallback gap, in steps that shrink with the
      // backlog, and the whole gesture lands: 14 lines asked for, 14 sent.
      act(() => {
        vi.advanceTimersByTime(200);
      });
      expect(notchesSent(forwardWheel)).toBe(14);
      act(() => {
        vi.advanceTimersByTime(500);
      });
      expect(notchesSent(forwardWheel)).toBe(14);
    });
  });
});

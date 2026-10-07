// @vitest-environment jsdom
// Rows sent to tmux come from the largest height seen per width, so a pane first measured with the keyboard
// up must defer its resize instead of shipping keyboard-shrunk rows, and a keyboard cycle never changes the grid.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CHAR_W, LINE_H, installResizeObserver, renderLiveTerminal, stubElementSize } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));

const WIDTH = 400;
const FULL_HEIGHT = 600;
const SHRUNK_HEIGHT = 250;
const GRID = [Math.floor(WIDTH / CHAR_W), Math.floor(FULL_HEIGHT / LINE_H)];

let clientHeight = FULL_HEIGHT;
stubElementSize({ clientWidth: () => WIDTH, clientHeight: () => clientHeight });
const observers = installResizeObserver();

beforeEach(() => {
  vi.useFakeTimers();
  observers.clear();
  clientHeight = FULL_HEIGHT;
});
afterEach(() => vi.useRealTimers());

function setKeyboard(view: ReturnType<typeof renderLiveTerminal>, open: boolean) {
  clientHeight = open ? SHRUNK_HEIGHT : FULL_HEIGHT;
  view.rerenderWith({ keyboardOpen: open });
  observers.settle();
}

describe("MobileLiveTerminal keyboard-aware sizing latch", () => {
  it("defers the first tmux resize while the keyboard has the container shrunk", () => {
    clientHeight = SHRUNK_HEIGHT;
    const view = renderLiveTerminal({ frame: null, keyboardOpen: true });
    observers.settle();
    expect(view.props.sendResize).not.toHaveBeenCalled();
    setKeyboard(view, false);
    expect((view.props.sendResize as ReturnType<typeof vi.fn>).mock.calls).toEqual([GRID]);
  });

  it("keeps the latched grid through a keyboard open and close cycle", () => {
    const view = renderLiveTerminal({ frame: null });
    observers.settle();
    setKeyboard(view, true);
    setKeyboard(view, false);
    const calls = (view.props.sendResize as ReturnType<typeof vi.fn>).mock.calls;
    expect(calls.length).toBeGreaterThan(0);
    for (const call of calls) expect(call).toEqual(GRID);
  });
});

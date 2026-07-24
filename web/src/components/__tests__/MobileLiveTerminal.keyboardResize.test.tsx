// @vitest-environment jsdom
//
// Regression for the keyboard-open sizing-latch seed bug: rows shipped to
// tmux come from a latch of the LARGEST container height seen per width, so
// a keyboard cycle never resizes tmux. But if the pane's FIRST measurement
// for a width happens while the soft keyboard has the container shrunk
// (mount with the keyboard up, rotation mid-cycle), the latch used to seed
// from the shrunk height and tmux got keyboard-shrunk rows, then a second
// resize on keyboard close. The `keyboardOpen` prop now defers seeding
// until the keyboard is gone; these tests pin the deferral, the correct
// late seed, and that a normal keyboard cycle still never changes the grid.

import { createRef } from "react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render } from "@testing-library/react";
import { MobileLiveTerminal } from "../MobileLiveTerminal";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));

// Mirror the component's grid math: charW falls back to fontSize * 0.6 in
// jsdom (the measure span has no layout), lineH is fontSize * LINE_RATIO.
const CHAR_W = 14 * 0.6;
const LINE_H = 14 * 1.2;
const WIDTH = 400;
const FULL_HEIGHT = 600;
const SHRUNK_HEIGHT = 250;
const COLS = Math.floor(WIDTH / CHAR_W);
const FULL_ROWS = Math.floor(FULL_HEIGHT / LINE_H);
const RESIZE_DEBOUNCE_MS = 150;

let clientHeight = FULL_HEIGHT;
let roCallbacks: Array<() => void> = [];

beforeAll(() => {
  globalThis.ResizeObserver = class {
    private cb: () => void;
    constructor(cb: () => void) {
      this.cb = cb;
    }
    observe() {
      roCallbacks.push(this.cb);
    }
    unobserve() {}
    disconnect() {
      roCallbacks = roCallbacks.filter((c) => c !== this.cb);
    }
  } as unknown as typeof ResizeObserver;

  Object.defineProperty(HTMLElement.prototype, "clientWidth", { configurable: true, get: () => WIDTH });
  Object.defineProperty(HTMLElement.prototype, "clientHeight", { configurable: true, get: () => clientHeight });
});

beforeEach(() => {
  vi.useFakeTimers();
  roCallbacks = [];
  clientHeight = FULL_HEIGHT;
});

afterEach(() => {
  vi.useRealTimers();
});

function props(keyboardOpen: boolean, sendResize = vi.fn()) {
  return {
    frame: null,
    connected: true,
    active: true,
    reading: false,
    sendResize,
    setWindow: vi.fn(),
    setCadence: vi.fn(),
    enterReading: vi.fn(),
    returnToLive: vi.fn(),
    sendData: vi.fn(),
    uploadPastedImage: vi.fn(),
    forwardWheel: vi.fn(),
    forwardButton: vi.fn(),
    ctrlActiveRef: createRef<boolean>() as React.RefObject<boolean>,
    clearCtrl: vi.fn(),
    inputRef: createRef<HTMLTextAreaElement>(),
    onInputFocusChange: vi.fn(),
    bottomAlign: true,
    keyboardOpen,
  };
}

/** Fire every registered ResizeObserver and let the compute debounce run. */
function settleLayout() {
  act(() => {
    for (const cb of [...roCallbacks]) cb();
    vi.advanceTimersByTime(RESIZE_DEBOUNCE_MS + 10);
  });
}

describe("MobileLiveTerminal keyboard-aware sizing latch", () => {
  it("defers the first tmux resize while the keyboard has the container shrunk", () => {
    const sendResize = vi.fn();
    clientHeight = SHRUNK_HEIGHT;
    const { rerender } = render(<MobileLiveTerminal {...props(true, sendResize)} />);
    settleLayout();
    expect(sendResize).not.toHaveBeenCalled();

    // Keyboard closes: the container grows back and the latch seeds from
    // the true no-keyboard height, so tmux gets full rows in ONE resize.
    clientHeight = FULL_HEIGHT;
    rerender(<MobileLiveTerminal {...props(false, sendResize)} />);
    settleLayout();
    expect(sendResize).toHaveBeenCalledTimes(1);
    expect(sendResize).toHaveBeenCalledWith(COLS, FULL_ROWS);
  });

  it("keeps the latched grid through a keyboard open/close cycle", () => {
    const sendResize = vi.fn();
    const { rerender } = render(<MobileLiveTerminal {...props(false, sendResize)} />);
    settleLayout();
    expect(sendResize).toHaveBeenLastCalledWith(COLS, FULL_ROWS);

    clientHeight = SHRUNK_HEIGHT;
    rerender(<MobileLiveTerminal {...props(true, sendResize)} />);
    settleLayout();
    clientHeight = FULL_HEIGHT;
    rerender(<MobileLiveTerminal {...props(false, sendResize)} />);
    settleLayout();

    // Every call carried the latched no-keyboard grid; the cycle never
    // shipped keyboard-shrunk rows.
    for (const call of sendResize.mock.calls) {
      expect(call).toEqual([COLS, FULL_ROWS]);
    }
  });
});

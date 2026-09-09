// @vitest-environment jsdom
//
// Select-to-copy on a streaming pane depends on the row DOM nodes surviving
// the frames that arrive while the selection is being made: a remounted row
// collapses the browser selection, and on iOS also dismisses the Copy callout.

import { createRef } from "react";
import { afterEach, beforeAll, beforeEach, expect, it, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";
import { MobileLiveTerminal } from "../MobileLiveTerminal";
import type { LiveFrame } from "../../hooks/useLiveTerminal";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));

// jsdom has no layout: charW falls back to fontSize * 0.6, so this width
// renders 28 columns once the sizing effect has run.
const WIDTH = 240;
const COLS = Math.floor(WIDTH / (14 * 0.6));
const RESIZE_DEBOUNCE_MS = 150;
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
  Object.defineProperty(HTMLElement.prototype, "clientHeight", { configurable: true, get: () => 600 });
});

beforeEach(() => {
  vi.useFakeTimers();
  roCallbacks = [];
});

afterEach(() => {
  vi.useRealTimers();
});

function frame(lines: string[], rows: number, history = Math.max(0, lines.length - rows)): LiveFrame {
  return {
    content: lines.join("\n") + "\n",
    lines,
    rows,
    history,
    cursor: null,
    altScreen: false,
    mouse: false,
    mouseSgr: false,
  };
}

function terminal(f: LiveFrame) {
  return (
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
      typedWordRef={{ current: "" }}
      uploadPastedImage={vi.fn()}
      forwardWheel={vi.fn()}
      forwardButton={vi.fn()}
      ctrlActiveRef={createRef<boolean>() as React.RefObject<boolean>}
      clearCtrl={vi.fn()}
      inputRef={createRef<HTMLTextAreaElement>()}
      onInputFocusChange={vi.fn()}
      bottomAlign
      keyboardOpen={false}
    />
  );
}

function rowCount(container: HTMLElement) {
  return container.querySelectorAll("[data-live-content] > div:not([aria-hidden])").length;
}

it("keeps unchanged row nodes when the agent appends lines", () => {
  const { rerender } = render(terminal(frame(["alpha", "beta", "$ "], 3)));
  const alpha = screen.getByText("alpha").parentElement;
  const beta = screen.getByText("beta").parentElement;

  rerender(terminal(frame(["alpha", "beta", "gamma", "delta", "$ "], 3)));

  expect(screen.getByText("alpha").parentElement).toBe(alpha);
  expect(screen.getByText("beta").parentElement).toBe(beta);
  expect(screen.getByText("delta")).toBeTruthy();
});

it("keeps row nodes when the window slides past wrapped lines", () => {
  // Two lines wider than the pane sit above the text being selected; the
  // live edge then slides the window by two lines, so those wrapped rows
  // drop off the top while the history count grows to match.
  const wide = (tag: string) => `${tag} `.repeat(20).trimEnd();
  const { container, rerender } = render(terminal(frame([wide("one"), wide("two"), "alpha", "beta", "$ "], 3)));
  act(() => {
    for (const cb of roCallbacks) cb();
    vi.advanceTimersByTime(RESIZE_DEBOUNCE_MS);
  });
  expect(COLS).toBe(28);
  expect(rowCount(container)).toBeGreaterThan(5);
  const alpha = screen.getByText("alpha").parentElement;

  rerender(terminal(frame(["alpha", "beta", "gamma", "delta", "$ "], 3, 4)));

  expect(screen.getByText("alpha").parentElement).toBe(alpha);
});

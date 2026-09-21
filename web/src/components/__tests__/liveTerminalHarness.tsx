import { createRef } from "react";
import { act, render } from "@testing-library/react";
import { vi } from "vitest";
import { MobileLiveTerminal, type MobileLiveTerminalProps } from "../MobileLiveTerminal";
import type { LiveFrame } from "../../hooks/useLiveTerminal";

// Grid math in jsdom: charW falls back to fontSize * 0.6 (no layout), lineH is fontSize * 1.2.
export const FONT_SIZE = 14;
export const CHAR_W = FONT_SIZE * 0.6;
export const LINE_H = FONT_SIZE * 1.2;
export const RESIZE_DEBOUNCE_MS = 150;

export function liveFrame(over: Partial<LiveFrame> = {}): LiveFrame {
  return {
    content: "$ \n",
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

/** A frame carrying explicit `lines`, with history defaulting to the lines above the screen. */
export function linesFrame(lines: string[], over: Partial<LiveFrame> & { rows: number }): LiveFrame {
  return liveFrame({
    content: lines.join("\n") + "\n",
    lines,
    history: Math.max(0, lines.length - over.rows),
    ...over,
  });
}

export const alt = { altScreen: true, mouse: true, mouseSgr: true } as const;

export function liveProps(over: Partial<MobileLiveTerminalProps> = {}): MobileLiveTerminalProps {
  return {
    frame: liveFrame(),
    connected: true,
    active: true,
    reading: false,
    sendResize: vi.fn(),
    setWindow: vi.fn(),
    setCadence: vi.fn(),
    enterReading: vi.fn(),
    returnToLive: vi.fn(),
    sendData: vi.fn(),
    typedWordRef: { current: "" },
    uploadPastedImage: vi.fn(async () => null),
    forwardWheel: vi.fn(),
    forwardButton: vi.fn(),
    ctrlActiveRef: { current: false },
    clearCtrl: vi.fn(),
    inputRef: createRef<HTMLTextAreaElement>(),
    onInputFocusChange: vi.fn(),
    bottomAlign: true,
    keyboardOpen: false,
    ...over,
  };
}

export function renderLiveTerminal(over: Partial<MobileLiveTerminalProps> = {}) {
  const props = liveProps(over);
  const utils = render(<MobileLiveTerminal {...props} />);
  return {
    ...utils,
    props,
    scroller: utils.container.querySelector("[data-live-terminal] > div") as HTMLElement,
    input: () => props.inputRef.current!,
    content: () => utils.container.querySelector("[data-live-content]") as HTMLElement,
    /** Rendered rows, excluding virtualization pads. */
    rowCount: () => utils.container.querySelectorAll("[data-live-content] > div:not([aria-hidden])").length,
    rerenderWith: (next: Partial<MobileLiveTerminalProps>) =>
      utils.rerender(<MobileLiveTerminal {...props} {...next} />),
  };
}

/** Installs a ResizeObserver whose callbacks tests fire by hand. */
export function installResizeObserver() {
  const callbacks = new Set<() => void>();
  globalThis.ResizeObserver = class {
    private cb: () => void;
    constructor(cb: () => void) {
      this.cb = cb;
    }
    observe() {
      callbacks.add(this.cb);
    }
    unobserve() {}
    disconnect() {
      callbacks.delete(this.cb);
    }
  } as unknown as typeof ResizeObserver;
  return {
    /** Fires every observer and lets the resize debounce run (fake timers). */
    settle: (ms = RESIZE_DEBOUNCE_MS + 10) =>
      act(() => {
        for (const cb of [...callbacks]) cb();
        vi.advanceTimersByTime(ms);
      }),
    clear: () => callbacks.clear(),
  };
}

export function stubElementSize(size: { clientWidth?: () => number; clientHeight?: () => number }) {
  for (const [prop, get] of Object.entries(size)) {
    Object.defineProperty(HTMLElement.prototype, prop, { configurable: true, get });
  }
}

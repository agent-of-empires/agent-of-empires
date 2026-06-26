// @vitest-environment jsdom
//
// Regression for the mobile "herky-jerky before scroll activates" bug: once the
// user nudges UP off the live edge, an arriving streaming frame must NOT snap the
// scroller back to the bottom. The live-edge auto-follow pin used a per-frame
// "moving up since the last mutation" test that re-attached on any single still
// frame, so a small scroll-up that paused inside the at-bottom tolerance got
// yanked back down on the next frame. That fight is the herky-jerky stutter.
//
// The pin now uses a sticky "detached from the live tail" latch. These tests
// pin down all three contracts: the latch must hold a reader's position while
// frames stream, must still follow the live tail when the user has NOT scrolled
// away, and must re-attach once the user returns to the bottom.

import { createRef } from "react";
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { MobileLiveTerminal } from "../MobileLiveTerminal";
import type { LiveFrame } from "../../hooks/useLiveTerminal";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));

const CLIENT_HEIGHT = 200;
const LINE_H = 14 * 1.2; // fontSize * LINE_RATIO

// scrollHeight is mutable so a test can simulate appended output growing the
// document (and thus the live-edge target).
let scrollHeight = 1000;
const bottom = () => scrollHeight - CLIENT_HEIGHT;

const scrollTopStore = new WeakMap<Element, number>();
let savedClientHeight: PropertyDescriptor | undefined;
let savedScrollHeight: PropertyDescriptor | undefined;
let savedScrollTop: PropertyDescriptor | undefined;

beforeAll(() => {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;

  savedClientHeight = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "clientHeight");
  savedScrollHeight = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollHeight");
  savedScrollTop = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollTop");
  Object.defineProperty(HTMLElement.prototype, "clientHeight", { configurable: true, get: () => CLIENT_HEIGHT });
  Object.defineProperty(HTMLElement.prototype, "scrollHeight", { configurable: true, get: () => scrollHeight });
  Object.defineProperty(HTMLElement.prototype, "scrollTop", {
    configurable: true,
    get() {
      return scrollTopStore.get(this) ?? 0;
    },
    set(v: number) {
      // Clamp to the scrollable range like a real scroller, so a content
      // shrink pulls scrollTop down to the new bottom (the case the latch
      // must not mistake for a user scroll-up).
      scrollTopStore.set(this, Math.max(0, Math.min(v, bottom())));
    },
  });
});

afterAll(() => {
  if (savedClientHeight) Object.defineProperty(HTMLElement.prototype, "clientHeight", savedClientHeight);
  if (savedScrollHeight) Object.defineProperty(HTMLElement.prototype, "scrollHeight", savedScrollHeight);
  if (savedScrollTop) Object.defineProperty(HTMLElement.prototype, "scrollTop", savedScrollTop);
});

beforeEach(() => {
  scrollHeight = 1000;
});

let frameSeq = 0;
function frame(): LiveFrame {
  // A fresh content string each call so `frame` identity changes and the
  // pinning layout effect re-runs, mimicking a streamed frame.
  frameSeq += 1;
  return {
    content: `frame ${frameSeq}\n`,
    rows: 3,
    history: 1000,
    cursor: null,
    altScreen: false,
    mouse: false,
    mouseSgr: false,
  };
}

function props() {
  return {
    frame: frame(),
    connected: true,
    active: true,
    reading: false,
    sendResize: vi.fn(),
    setWindow: vi.fn(),
    setCadence: vi.fn(),
    enterReading: vi.fn(),
    returnToLive: vi.fn(),
    sendData: vi.fn(),
    forwardWheel: vi.fn(),
    forwardButton: vi.fn(),
    ctrlActiveRef: createRef<boolean>() as React.RefObject<boolean>,
    clearCtrl: vi.fn(),
    inputRef: createRef<HTMLTextAreaElement>(),
    onInputFocusChange: vi.fn(),
    bottomAlign: true,
  };
}

function mount() {
  const { container, rerender } = render(<MobileLiveTerminal {...props()} />);
  const scroller = container.querySelector("[data-live-terminal] > div") as HTMLElement;
  const stream = () => rerender(<MobileLiveTerminal {...props()} />);
  return { scroller, stream };
}

describe("MobileLiveTerminal live-edge scroll", () => {
  it("does not snap a small scroll-up back to the bottom when frames keep streaming", () => {
    const { scroller, stream } = mount();
    expect(scroller.scrollTop).toBe(bottom()); // initial pin parks at the live edge

    // User nudges up less than 1.5 lines (inside the old dead zone) and stops.
    const readingPos = bottom() - LINE_H;
    scroller.scrollTop = readingPos;
    fireEvent.scroll(scroller);

    // Several streaming frames arrive while the finger is still (no touch).
    stream();
    stream();
    stream();

    expect(scroller.scrollTop).toBeCloseTo(readingPos, 0);
  });

  it("still follows the live tail when the user has not scrolled away", () => {
    const { scroller, stream } = mount();
    expect(scroller.scrollTop).toBe(bottom());

    // Output appends two lines: the document grows and the pin should follow.
    scrollHeight += 2 * LINE_H;
    stream();

    expect(scroller.scrollTop).toBe(bottom());
  });

  it("does not detach when a content shrink clamps scrollTop to the new bottom", () => {
    const { scroller, stream } = mount();
    expect(scroller.scrollTop).toBe(bottom());

    // Trailing blank rows trimmed: document shrinks, the browser clamps
    // scrollTop down to the new bottom (modelled by re-asserting scrollTop,
    // which the mock setter clamps to the scrollable range). That must read
    // as "still live", not as a user scroll-up, so the next appended line
    // keeps following.
    scrollHeight -= 2 * LINE_H;
    scroller.scrollTop = Math.min(scroller.scrollTop, bottom());
    stream();
    expect(scroller.scrollTop).toBe(bottom());

    scrollHeight += 5 * LINE_H;
    stream();
    expect(scroller.scrollTop).toBe(bottom());
  });

  it("re-attaches and follows again once the user scrolls back to the bottom", () => {
    const { scroller, stream } = mount();

    // Detach by reading scrollback well above the live edge.
    scroller.scrollTop = bottom() - 10 * LINE_H;
    fireEvent.scroll(scroller);
    stream();
    expect(scroller.scrollTop).toBeCloseTo(bottom() - 10 * LINE_H, 0); // held, not snapped

    // User scrolls back down to the bottom.
    scroller.scrollTop = bottom();
    fireEvent.scroll(scroller);
    stream();

    // Now appended output is followed again.
    scrollHeight += 3 * LINE_H;
    stream();
    expect(scroller.scrollTop).toBe(bottom());
  });
});

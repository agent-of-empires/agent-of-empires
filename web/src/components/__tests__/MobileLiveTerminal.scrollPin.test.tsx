// @vitest-environment jsdom
// Live-edge following uses a sticky detach latch: a paused scroll-up must not be snapped back by the next frame,
// while an undisturbed view still follows the tail and re-attaches once the reader returns to the bottom.

import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent } from "@testing-library/react";
import { LINE_H, installResizeObserver, liveFrame, renderLiveTerminal } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));
installResizeObserver();

const CLIENT_HEIGHT = 200;
let scrollHeight = 1000;
const bottom = () => scrollHeight - CLIENT_HEIGHT;
const scrollTops = new WeakMap<Element, number>();
const saved = ["clientHeight", "scrollHeight", "scrollTop"].map(
  (p) => [p, Object.getOwnPropertyDescriptor(HTMLElement.prototype, p)] as const,
);

beforeAll(() => {
  const define = (p: string, d: PropertyDescriptor) =>
    Object.defineProperty(HTMLElement.prototype, p, { configurable: true, ...d });
  define("clientHeight", { get: () => CLIENT_HEIGHT });
  define("scrollHeight", { get: () => scrollHeight });
  // Clamped like a real scroller, so a content shrink pulls scrollTop to the new bottom.
  define("scrollTop", {
    get() {
      return scrollTops.get(this) ?? 0;
    },
    set(v: number) {
      scrollTops.set(this, Math.max(0, Math.min(v, bottom())));
    },
  });
});
afterAll(() => {
  for (const [p, d] of saved) if (d) Object.defineProperty(HTMLElement.prototype, p, d);
});
beforeEach(() => {
  scrollHeight = 1000;
});

let seq = 0;
/** A fresh frame each call, so the pin effect re-runs like a streamed frame. */
const nextFrame = () => liveFrame({ content: `frame ${++seq}\n` });

function mount(over = {}) {
  const view = renderLiveTerminal({ frame: nextFrame(), ...over });
  const stream = () => view.rerenderWith({ frame: nextFrame() });
  const scrollTo = (top: number) => {
    view.scroller.scrollTop = top;
    fireEvent.scroll(view.scroller);
  };
  return { ...view, stream, scrollTo };
}

describe("MobileLiveTerminal live-edge scroll", () => {
  it("holds a small paused scroll-up while frames stream", () => {
    const { scroller, stream, scrollTo } = mount();
    expect(scroller.scrollTop).toBe(bottom());
    scrollTo(bottom() - LINE_H);
    stream();
    stream();
    stream();
    expect(scroller.scrollTop).toBeCloseTo(bottom() - LINE_H, 0);
  });

  it("follows appended output when the user has not scrolled away", () => {
    const { scroller, stream } = mount();
    scrollHeight += 2 * LINE_H;
    stream();
    expect(scroller.scrollTop).toBe(bottom());
  });

  it("does not detach when a content shrink clamps scrollTop", () => {
    const { scroller, stream } = mount();
    scrollHeight -= 2 * LINE_H;
    scroller.scrollTop = Math.min(scroller.scrollTop, bottom());
    stream();
    expect(scroller.scrollTop).toBe(bottom());
    scrollHeight += 5 * LINE_H;
    stream();
    expect(scroller.scrollTop).toBe(bottom());
  });

  it("re-attaches once the user scrolls back to the bottom", () => {
    const { scroller, stream, scrollTo } = mount();
    scrollTo(bottom() - 10 * LINE_H);
    stream();
    expect(scroller.scrollTop).toBeCloseTo(bottom() - 10 * LINE_H, 0);
    scrollTo(bottom());
    stream();
    scrollHeight += 3 * LINE_H;
    stream();
    expect(scroller.scrollTop).toBe(bottom());
  });

  it("keeps returning to live when the keyboard scroll races the reading-state update", async () => {
    const { props, scrollTo } = mount({ reading: true, keyboardOpen: true });
    // The keyboard effect arms the force-live guard; this scroll arrives while `reading` is still stale.
    await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
    scrollTo(bottom() - 10 * LINE_H);
    expect(props.enterReading).not.toHaveBeenCalled();
    expect(props.returnToLive).toHaveBeenCalledTimes(2);
  });
});

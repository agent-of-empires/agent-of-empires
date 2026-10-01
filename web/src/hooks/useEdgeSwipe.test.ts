// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useEdgeSwipe } from "./useEdgeSwipe";

const ORIGINAL_WIDTH = window.innerWidth;

function setWidth(px: number) {
  Object.defineProperty(window, "innerWidth", { value: px, configurable: true, writable: true });
}

type Point = [x: number, y: number];

function dispatchTouch(type: string, points: Point[]) {
  const ev = new Event(type) as Event & { touches: { clientX: number; clientY: number }[] };
  ev.touches = points.map(([clientX, clientY]) => ({ clientX, clientY }));
  window.dispatchEvent(ev);
}

/** Start a one-finger touch at `start`, then move through `moves`. */
function swipe(start: Point, ...moves: Point[]) {
  dispatchTouch("touchstart", [start]);
  for (const m of moves) dispatchTouch("touchmove", [m]);
}

type Options = Parameters<typeof useEdgeSwipe>[0];

function mount(over: Partial<Options> = {}) {
  const onSwipe = vi.fn();
  const hook = renderHook(() => useEdgeSwipe({ edge: "left", enabled: true, onSwipe, ...over }));
  return { onSwipe, ...hook };
}

beforeEach(() => setWidth(400));

afterEach(() => {
  setWidth(ORIGINAL_WIDTH);
  vi.restoreAllMocks();
});

describe("useEdgeSwipe", () => {
  it.each<[string, Partial<Options>, Point, Point[], number]>([
    [
      "left edge fires once across further moves",
      {},
      [5, 100],
      [
        [80, 100],
        [120, 100],
      ],
      1,
    ],
    ["left edge outside the edge zone", {}, [200, 100], [[300, 100]], 0],
    ["left edge below the threshold", {}, [5, 100], [[50, 100]], 0],
    ["right edge past the threshold", { edge: "right" }, [390, 100], [[320, 100]], 1],
    ["right edge away from the edge", { edge: "right" }, [200, 100], [[100, 100]], 0],
    [
      "a gesture that turns vertical",
      {},
      [5, 100],
      [
        [10, 160],
        [200, 160],
      ],
      0,
    ],
    ["anywhere mode below 90px", { anywhere: true }, [200, 100], [[270, 100]], 0],
    [
      "anywhere mode past 90px",
      { anywhere: true },
      [200, 100],
      [
        [270, 100],
        [300, 100],
      ],
      1,
    ],
    ["anywhere mode from the system-back strip", { anywhere: true }, [8, 100], [[180, 100]], 0],
    ["disabled", { enabled: false }, [5, 100], [[200, 100]], 0],
  ])("%s", (_label, options, start, moves, calls) => {
    const { onSwipe } = mount(options);
    swipe(start, ...moves);
    expect(onSwipe).toHaveBeenCalledTimes(calls);
  });

  it("does nothing on desktop widths", () => {
    setWidth(1024);
    const { onSwipe } = mount();
    swipe([5, 100], [200, 100]);
    expect(onSwipe).not.toHaveBeenCalled();
  });

  it("ignores multi-finger gestures, stray moves after touchend, and unmounted hooks", () => {
    const { onSwipe, unmount } = mount();
    dispatchTouch("touchstart", [
      [5, 100],
      [6, 100],
    ]);
    dispatchTouch("touchmove", [[200, 100]]);
    dispatchTouch("touchstart", [[5, 100]]);
    dispatchTouch("touchend", []);
    dispatchTouch("touchmove", [[200, 100]]);
    unmount();
    swipe([5, 100], [200, 100]);
    expect(onSwipe).not.toHaveBeenCalled();
  });

  it.each<[string, string, number, number]>([
    ["an overflowing scroller", "auto", 300, 1],
    ["a scroller with nothing to scroll", "auto", 100, 0],
    ["a non-scrolling overflow", "visible", 300, 0],
  ])("anywhere mode starting in %s", (_label, overflowX, scrollWidth, ignored) => {
    const pre = document.createElement("pre");
    pre.style.overflowX = overflowX;
    Object.defineProperty(pre, "clientWidth", { value: 100 });
    Object.defineProperty(pre, "scrollWidth", { value: scrollWidth });
    const code = document.createElement("code");
    pre.appendChild(code);
    document.body.appendChild(pre);

    const { onSwipe } = mount({ anywhere: true });
    const start = new Event("touchstart", { bubbles: true }) as Event & { touches: unknown[] };
    start.touches = [{ clientX: 200, clientY: 100 }];
    code.dispatchEvent(start);
    dispatchTouch("touchmove", [[200 + 100, 100]]);
    expect(onSwipe).toHaveBeenCalledTimes(ignored ? 0 : 1);
    pre.remove();
  });

  it("blurs the active element before invoking onSwipe", () => {
    const input = document.createElement("input");
    document.body.appendChild(input);
    input.focus();
    const blurSpy = vi.spyOn(input, "blur");
    const { onSwipe } = mount({ blurOnSwipe: true });
    swipe([5, 100], [80, 100]);
    expect(blurSpy).toHaveBeenCalled();
    expect(onSwipe).toHaveBeenCalledTimes(1);
    input.remove();
  });
});

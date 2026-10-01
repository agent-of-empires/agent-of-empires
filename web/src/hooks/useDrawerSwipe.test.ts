// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { drawerSwipeAction, useDrawerSwipe, type DrawerSwipeState, type SwipeDirection } from "./useDrawerSwipe";

const ORIGINAL_WIDTH = window.innerWidth;

function setWidth(px: number) {
  Object.defineProperty(window, "innerWidth", { value: px, configurable: true, writable: true });
}

type Point = [x: number, y: number];

function dispatchTouch(type: string, points: Point[], target: EventTarget = window) {
  const ev = new Event(type, { bubbles: true }) as Event & { touches: { clientX: number; clientY: number }[] };
  ev.touches = points.map(([clientX, clientY]) => ({ clientX, clientY }));
  target.dispatchEvent(ev);
}

/** Start a one-finger touch at `start`, then move through `moves`. */
function swipe(start: Point, ...moves: Point[]) {
  dispatchTouch("touchstart", [start]);
  for (const m of moves) dispatchTouch("touchmove", [m]);
}

/** A 200px-wide `overflow-x: auto` box holding 600px of content, scrolled to `scrollLeft`. */
function horizontalScroller(scrollLeft: number) {
  const el = document.createElement("div");
  el.style.overflowX = "auto";
  Object.defineProperties(el, {
    scrollWidth: { value: 600 },
    clientWidth: { value: 200 },
    scrollLeft: { value: scrollLeft },
  });
  document.body.appendChild(el);
  return el;
}

const CLOSED: DrawerSwipeState = { sidebarOpen: false, sidebarSide: "left", panelsOpen: false, panelsAvailable: true };

function mount(state: Partial<DrawerSwipeState> = {}) {
  const onAction = vi.fn();
  const hook = renderHook(() => useDrawerSwipe({ ...CLOSED, ...state }, onAction));
  return { onAction, ...hook };
}

beforeEach(() => setWidth(400));

afterEach(() => {
  setWidth(ORIGINAL_WIDTH);
  vi.restoreAllMocks();
});

describe("drawerSwipeAction", () => {
  it.each<[string, SwipeDirection, Partial<DrawerSwipeState>, ReturnType<typeof drawerSwipeAction>]>([
    ["swipe right opens the left sidebar", "right", {}, "open-sidebar"],
    ["swipe left opens the panels", "left", {}, "open-panels"],
    ["swipe left without a session does nothing", "left", { panelsAvailable: false }, null],
    ["swipe left closes the left sidebar", "left", { sidebarOpen: true }, "close-sidebar"],
    ["swipe right on an open left sidebar does nothing", "right", { sidebarOpen: true }, null],
    ["swipe right closes the panels", "right", { panelsOpen: true }, "close-panels"],
    ["swipe left on open panels does nothing", "left", { panelsOpen: true }, null],
    ["a right-side sidebar has no open swipe", "right", { sidebarSide: "right" }, null],
    ["swipe right closes a right-side sidebar", "right", { sidebarSide: "right", sidebarOpen: true }, "close-sidebar"],
  ])("%s", (_label, dir, state, expected) => {
    expect(drawerSwipeAction(dir, { ...CLOSED, ...state })).toBe(expected);
  });
});

describe("useDrawerSwipe", () => {
  it.each<[string, Point, Point[], string | null]>([
    [
      "right swipe from mid-screen",
      [150, 100],
      [
        [200, 100],
        [250, 100],
      ],
      "open-sidebar",
    ],
    [
      "left swipe from mid-screen",
      [250, 100],
      [
        [200, 100],
        [150, 100],
      ],
      "open-panels",
    ],
    ["below the threshold", [150, 100], [[230, 100]], null],
    ["from the left system strip", [8, 100], [[180, 100]], null],
    ["from the right system strip", [392, 100], [[220, 100]], null],
    [
      "vertical drift cancels",
      [150, 100],
      [
        [155, 160],
        [300, 160],
      ],
      null,
    ],
  ])("%s", (_label, start, moves, action) => {
    const { onAction } = mount();
    swipe(start, ...moves);
    if (action) expect(onAction).toHaveBeenCalledExactlyOnceWith(action);
    else expect(onAction).not.toHaveBeenCalled();
  });

  it.each<[string, number, Point, Point, string | null]>([
    ["left swipe at the scroll start scrolls instead", 0, [300, 100], [150, 100], null],
    ["right swipe at the scroll start opens the sidebar", 0, [150, 100], [300, 100], "open-sidebar"],
    ["right swipe mid-scroll scrolls instead", 200, [150, 100], [300, 100], null],
    ["left swipe at the scroll end opens the panels", 400, [300, 100], [150, 100], "open-panels"],
  ])("inside a horizontal scroller: %s", (_label, scrollLeft, start, end, action) => {
    const el = horizontalScroller(scrollLeft);
    const { onAction } = mount();
    dispatchTouch("touchstart", [start], el);
    dispatchTouch("touchmove", [end], el);
    el.remove();
    if (action) expect(onAction).toHaveBeenCalledExactlyOnceWith(action);
    else expect(onAction).not.toHaveBeenCalled();
  });

  it("acts on the latest state without remounting", () => {
    const onAction = vi.fn();
    const { rerender } = renderHook(({ state }) => useDrawerSwipe(state, onAction), {
      initialProps: { state: CLOSED },
    });
    rerender({ state: { ...CLOSED, panelsOpen: true } });
    swipe([150, 100], [300, 100]);
    expect(onAction).toHaveBeenCalledExactlyOnceWith("close-panels");
  });

  it("does nothing on desktop widths", () => {
    setWidth(1024);
    const { onAction } = mount();
    swipe([150, 100], [400, 100]);
    expect(onAction).not.toHaveBeenCalled();
  });

  it("ignores multi-finger gestures, stray moves after touchend, and unmounted hooks", () => {
    const { onAction, unmount } = mount();
    dispatchTouch("touchstart", [
      [150, 100],
      [160, 100],
    ]);
    dispatchTouch("touchmove", [[300, 100]]);
    dispatchTouch("touchstart", [[150, 100]]);
    dispatchTouch("touchend", []);
    dispatchTouch("touchmove", [[300, 100]]);
    unmount();
    swipe([150, 100], [300, 100]);
    expect(onAction).not.toHaveBeenCalled();
  });

  it("blurs the active element before acting, but not when the swipe maps to nothing", () => {
    const input = document.createElement("input");
    document.body.appendChild(input);
    input.focus();
    const blurSpy = vi.spyOn(input, "blur");
    mount({ panelsAvailable: false });
    swipe([250, 100], [100, 100]);
    expect(blurSpy).not.toHaveBeenCalled();
    swipe([100, 100], [250, 100]);
    expect(blurSpy).toHaveBeenCalledTimes(1);
    input.remove();
  });
});

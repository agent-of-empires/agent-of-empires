// @vitest-environment jsdom
//
// Hook tests for useEdgeSwipe. The hook attaches window touch listeners and
// fires onSwipe when a one-finger gesture crosses a horizontal threshold:
//   - edge-zone start required by default (left <= 24px, right >= w-24px)
//   - 60px threshold (90px in `anywhere` mode)
//   - cancels if the gesture is dominantly vertical past 16px
//   - mobile-only (window width < 768)
// jsdom has no real touch, so TouchEvents are dispatched on window with a
// hand-rolled `touches` array. Events fire ~1ms apart but the hook has no
// velocity math, so back-to-back dispatch is fine.

import { renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useEdgeSwipe } from "./useEdgeSwipe";

const ORIGINAL_WIDTH = window.innerWidth;

function setWidth(px: number) {
  Object.defineProperty(window, "innerWidth", { value: px, configurable: true, writable: true });
}

// Build a touch-like list. jsdom's TouchEvent constructor ignores `touches`,
// so dispatch a plain Event and attach the list ourselves.
function dispatchTouch(type: string, points: Array<{ clientX: number; clientY: number }>) {
  const ev = new Event(type) as unknown as { touches: typeof points } & Event;
  ev.touches = points;
  window.dispatchEvent(ev as Event);
}

beforeEach(() => {
  setWidth(400); // mobile by default
});

afterEach(() => {
  setWidth(ORIGINAL_WIDTH);
  vi.restoreAllMocks();
});

describe("useEdgeSwipe left edge", () => {
  it("fires onSwipe when a left-edge swipe crosses the threshold", () => {
    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "left", enabled: true, onSwipe }));

    dispatchTouch("touchstart", [{ clientX: 10, clientY: 100 }]);
    dispatchTouch("touchmove", [{ clientX: 80, clientY: 105 }]); // dx = 70 > 60
    expect(onSwipe).toHaveBeenCalledTimes(1);
  });

  it("does not start tracking when the touch begins outside the edge zone", () => {
    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "left", enabled: true, onSwipe }));

    dispatchTouch("touchstart", [{ clientX: 200, clientY: 100 }]); // > 24px in
    dispatchTouch("touchmove", [{ clientX: 300, clientY: 100 }]);
    expect(onSwipe).not.toHaveBeenCalled();
  });

  it("does not fire below the horizontal threshold", () => {
    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "left", enabled: true, onSwipe }));

    dispatchTouch("touchstart", [{ clientX: 5, clientY: 100 }]);
    dispatchTouch("touchmove", [{ clientX: 50, clientY: 100 }]); // dx = 45 < 60
    expect(onSwipe).not.toHaveBeenCalled();
  });

  it("fires only once even if more moves cross the threshold", () => {
    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "left", enabled: true, onSwipe }));

    dispatchTouch("touchstart", [{ clientX: 5, clientY: 100 }]);
    dispatchTouch("touchmove", [{ clientX: 80, clientY: 100 }]);
    dispatchTouch("touchmove", [{ clientX: 120, clientY: 100 }]);
    expect(onSwipe).toHaveBeenCalledTimes(1);
  });
});

describe("useEdgeSwipe right edge", () => {
  it("fires onSwipe when a right-edge swipe crosses the threshold", () => {
    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "right", enabled: true, onSwipe }));

    // start within w-24 = 376px..400px
    dispatchTouch("touchstart", [{ clientX: 390, clientY: 100 }]);
    dispatchTouch("touchmove", [{ clientX: 320, clientY: 100 }]); // dx = startX - x = 70 > 60
    expect(onSwipe).toHaveBeenCalledTimes(1);
  });

  it("ignores a right-edge touch that starts away from the edge", () => {
    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "right", enabled: true, onSwipe }));

    dispatchTouch("touchstart", [{ clientX: 200, clientY: 100 }]);
    dispatchTouch("touchmove", [{ clientX: 100, clientY: 100 }]);
    expect(onSwipe).not.toHaveBeenCalled();
  });
});

describe("useEdgeSwipe vertical cancel", () => {
  it("cancels tracking when the gesture turns dominantly vertical", () => {
    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "left", enabled: true, onSwipe }));

    dispatchTouch("touchstart", [{ clientX: 5, clientY: 100 }]);
    // dy dominant and > 16 -> tracking stops
    dispatchTouch("touchmove", [{ clientX: 10, clientY: 160 }]);
    // even a subsequent big horizontal move must not fire
    dispatchTouch("touchmove", [{ clientX: 200, clientY: 160 }]);
    expect(onSwipe).not.toHaveBeenCalled();
  });
});

describe("useEdgeSwipe anywhere mode", () => {
  it("starts mid-screen and needs the larger 90px threshold", () => {
    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "left", enabled: true, onSwipe, anywhere: true }));

    dispatchTouch("touchstart", [{ clientX: 200, clientY: 100 }]); // mid-screen ok
    dispatchTouch("touchmove", [{ clientX: 270, clientY: 100 }]); // dx = 70 < 90, no fire
    expect(onSwipe).not.toHaveBeenCalled();

    dispatchTouch("touchmove", [{ clientX: 300, clientY: 100 }]); // dx = 100 > 90
    expect(onSwipe).toHaveBeenCalledTimes(1);
  });
});

describe("useEdgeSwipe blurOnSwipe", () => {
  it("blurs the active element before invoking onSwipe", () => {
    const input = document.createElement("input");
    document.body.appendChild(input);
    input.focus();
    expect(document.activeElement).toBe(input);
    const blurSpy = vi.spyOn(input, "blur");

    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "left", enabled: true, onSwipe, blurOnSwipe: true }));

    dispatchTouch("touchstart", [{ clientX: 5, clientY: 100 }]);
    dispatchTouch("touchmove", [{ clientX: 80, clientY: 100 }]);

    expect(blurSpy).toHaveBeenCalled();
    expect(onSwipe).toHaveBeenCalledTimes(1);
    input.remove();
  });
});

describe("useEdgeSwipe guards", () => {
  it("does nothing when disabled", () => {
    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "left", enabled: false, onSwipe }));

    dispatchTouch("touchstart", [{ clientX: 5, clientY: 100 }]);
    dispatchTouch("touchmove", [{ clientX: 200, clientY: 100 }]);
    expect(onSwipe).not.toHaveBeenCalled();
  });

  it("does nothing on desktop widths (>= 768px)", () => {
    setWidth(1024);
    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "left", enabled: true, onSwipe }));

    dispatchTouch("touchstart", [{ clientX: 5, clientY: 100 }]);
    dispatchTouch("touchmove", [{ clientX: 200, clientY: 100 }]);
    expect(onSwipe).not.toHaveBeenCalled();
  });

  it("ignores multi-finger gestures", () => {
    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "left", enabled: true, onSwipe }));

    dispatchTouch("touchstart", [
      { clientX: 5, clientY: 100 },
      { clientX: 6, clientY: 100 },
    ]);
    dispatchTouch("touchmove", [{ clientX: 200, clientY: 100 }]);
    expect(onSwipe).not.toHaveBeenCalled();
  });

  it("touchend resets tracking so a later stray move does not fire", () => {
    const onSwipe = vi.fn();
    renderHook(() => useEdgeSwipe({ edge: "left", enabled: true, onSwipe }));

    dispatchTouch("touchstart", [{ clientX: 5, clientY: 100 }]);
    dispatchTouch("touchend", []);
    dispatchTouch("touchmove", [{ clientX: 200, clientY: 100 }]);
    expect(onSwipe).not.toHaveBeenCalled();
  });

  it("removes window listeners on unmount", () => {
    const onSwipe = vi.fn();
    const { unmount } = renderHook(() => useEdgeSwipe({ edge: "left", enabled: true, onSwipe }));
    unmount();

    dispatchTouch("touchstart", [{ clientX: 5, clientY: 100 }]);
    dispatchTouch("touchmove", [{ clientX: 200, clientY: 100 }]);
    expect(onSwipe).not.toHaveBeenCalled();
  });
});

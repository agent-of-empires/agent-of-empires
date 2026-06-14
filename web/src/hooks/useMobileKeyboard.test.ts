// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";

import { useMobileKeyboard } from "./useMobileKeyboard";

type Listener = (...args: unknown[]) => void;

// A minimal matchMedia stub that lets a test flip `pointer: coarse`.
function stubMatchMedia(initialCoarse: boolean) {
  let matches = initialCoarse;
  const listeners = new Set<Listener>();
  const mql = {
    get matches() {
      return matches;
    },
    media: "(pointer: coarse)",
    addEventListener: (_: string, cb: Listener) => listeners.add(cb),
    removeEventListener: (_: string, cb: Listener) => listeners.delete(cb),
  };
  window.matchMedia = vi.fn().mockReturnValue(mql) as unknown as typeof window.matchMedia;
  return {
    set(next: boolean) {
      matches = next;
      listeners.forEach((cb) => cb());
    },
    listenerCount: () => listeners.size,
  };
}

// A controllable visualViewport. height is mutable; resize/scroll fire the
// registered listeners synchronously.
function stubVisualViewport(initialHeight: number) {
  const listeners = new Map<string, Set<Listener>>();
  const vv = {
    height: initialHeight,
    addEventListener: (type: string, cb: Listener) => {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type)!.add(cb);
    },
    removeEventListener: (type: string, cb: Listener) => {
      listeners.get(type)?.delete(cb);
    },
  };
  Object.defineProperty(window, "visualViewport", {
    configurable: true,
    value: vv,
  });
  return {
    vv,
    setHeight(h: number) {
      vv.height = h;
    },
    fire(type: string) {
      listeners.get(type)?.forEach((cb) => cb());
    },
    listenerCount(type: string) {
      return listeners.get(type)?.size ?? 0;
    },
  };
}

let rafQueue: FrameRequestCallback[] = [];

beforeEach(() => {
  rafQueue = [];
  // Synchronous-but-controlled rAF so polling loops are drainable.
  vi.spyOn(window, "requestAnimationFrame").mockImplementation((cb: FrameRequestCallback) => {
    rafQueue.push(cb);
    return rafQueue.length;
  });
  vi.spyOn(window, "cancelAnimationFrame").mockImplementation(() => {});
  // Tall layout viewport; keyboard shrinks the visual viewport below it.
  Object.defineProperty(window, "innerHeight", { configurable: true, value: 800, writable: true });
  window.scrollTo = vi.fn();
});

afterEach(() => {
  vi.restoreAllMocks();
  // @ts-expect-error cleanup
  delete window.visualViewport;
});

// Drain the rAF poll queue a bounded number of times.
function drainRaf(rounds = 30) {
  for (let i = 0; i < rounds && rafQueue.length > 0; i++) {
    const next = rafQueue.shift()!;
    next(performance.now());
  }
}

describe("useMobileKeyboard", () => {
  it("reports mobile from an initial coarse pointer", () => {
    stubMatchMedia(true);
    stubVisualViewport(800);
    const { result } = renderHook(() => useMobileKeyboard());
    expect(result.current.isMobile).toBe(true);
    expect(result.current.keyboardOpen).toBe(false);
    expect(result.current.keyboardHeight).toBe(0);
  });

  it("is a no-op on the desktop (fine pointer) path", () => {
    stubMatchMedia(false);
    const ctl = stubVisualViewport(800);
    const { result } = renderHook(() => useMobileKeyboard());
    expect(result.current.isMobile).toBe(false);
    // The viewport effect early-returns, so no resize/scroll listeners are wired.
    expect(ctl.listenerCount("resize")).toBe(0);
    expect(ctl.listenerCount("scroll")).toBe(0);
  });

  it("detects the keyboard opening and measures the bottom inset", () => {
    stubMatchMedia(true);
    const vp = stubVisualViewport(800);

    const { result } = renderHook(() => useMobileKeyboard());
    expect(result.current.keyboardOpen).toBe(false);

    // Keyboard occludes 300px of the 800px viewport.
    act(() => {
      vp.setHeight(500);
      vp.fire("resize");
      drainRaf();
    });

    expect(result.current.keyboardOpen).toBe(true);
    // padding = innerHeight(800) - vvHeight(500) - safeBottom(0) = 300
    expect(result.current.keyboardHeight).toBe(300);
  });

  it("ignores a small URL-bar nudge (under the 100px threshold)", () => {
    stubMatchMedia(true);
    const vp = stubVisualViewport(800);

    const { result } = renderHook(() => useMobileKeyboard());

    act(() => {
      vp.setHeight(760); // 40px occlusion, below the keyboard threshold
      vp.fire("resize");
      drainRaf();
    });

    expect(result.current.keyboardOpen).toBe(false);
    expect(result.current.keyboardHeight).toBe(0);
  });

  it("flips back to closed when the keyboard is dismissed", () => {
    stubMatchMedia(true);
    const vp = stubVisualViewport(800);

    const { result } = renderHook(() => useMobileKeyboard());

    act(() => {
      vp.setHeight(500);
      vp.fire("resize");
      drainRaf();
    });
    expect(result.current.keyboardOpen).toBe(true);

    act(() => {
      vp.setHeight(800);
      vp.fire("resize");
      drainRaf();
    });
    expect(result.current.keyboardOpen).toBe(false);
    expect(result.current.keyboardHeight).toBe(0);
  });

  it("snaps stray layout-viewport scroll back to the top on measure", () => {
    stubMatchMedia(true);
    const vp = stubVisualViewport(800);
    Object.defineProperty(window, "scrollY", { configurable: true, value: 120, writable: true });

    renderHook(() => useMobileKeyboard());

    act(() => {
      vp.fire("scroll");
      drainRaf();
    });

    expect(window.scrollTo).toHaveBeenCalledWith(0, 0);
  });

  it("starts polling when a text input gains focus", () => {
    stubMatchMedia(true);
    const vp = stubVisualViewport(800);

    const { result } = renderHook(() => useMobileKeyboard());

    const input = document.createElement("input");
    document.body.appendChild(input);

    act(() => {
      vp.setHeight(500);
      input.dispatchEvent(new FocusEvent("focusin", { bubbles: true }));
      drainRaf();
    });

    expect(result.current.keyboardOpen).toBe(true);
    input.remove();
  });

  it("becomes mobile when matchMedia later reports coarse, then clears on leaving", () => {
    const mq = stubMatchMedia(false);
    stubVisualViewport(800);

    const { result } = renderHook(() => useMobileKeyboard());
    expect(result.current.isMobile).toBe(false);

    act(() => mq.set(true));
    expect(result.current.isMobile).toBe(true);

    act(() => mq.set(false));
    expect(result.current.isMobile).toBe(false);
    expect(result.current.keyboardOpen).toBe(false);
    expect(result.current.keyboardHeight).toBe(0);
  });

  it("removes all viewport listeners on unmount", () => {
    stubMatchMedia(true);
    const vp = stubVisualViewport(800);
    const docRemove = vi.spyOn(document, "removeEventListener");
    const winRemove = vi.spyOn(window, "removeEventListener");

    const { unmount } = renderHook(() => useMobileKeyboard());
    expect(vp.listenerCount("resize")).toBe(1);
    expect(vp.listenerCount("scroll")).toBe(1);

    unmount();

    expect(vp.listenerCount("resize")).toBe(0);
    expect(vp.listenerCount("scroll")).toBe(0);
    expect(docRemove).toHaveBeenCalledWith("focusin", expect.any(Function));
    expect(winRemove).toHaveBeenCalledWith("orientationchange", expect.any(Function));
    expect(winRemove).toHaveBeenCalledWith("scroll", expect.any(Function));
  });
});

// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";

import { useMobileKeyboard } from "./useMobileKeyboard";
import { stubMatchMedia } from "./__tests__/fixtures";

type Listener = (...args: unknown[]) => void;

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
  vi.spyOn(window, "requestAnimationFrame").mockImplementation((cb: FrameRequestCallback) => {
    rafQueue.push(cb);
    return rafQueue.length;
  });
  vi.spyOn(window, "cancelAnimationFrame").mockImplementation(() => {});
  Object.defineProperty(window, "innerHeight", { configurable: true, value: 800, writable: true });
  window.scrollTo = vi.fn();
});

afterEach(() => {
  vi.restoreAllMocks();
  // @ts-expect-error cleanup
  delete window.visualViewport;
});

function drainRaf(rounds = 30) {
  for (let i = 0; i < rounds && rafQueue.length > 0; i++) {
    const next = rafQueue.shift()!;
    next(performance.now());
  }
}

function mountMobile(height = 800) {
  stubMatchMedia(true, "(pointer: coarse)");
  const vp = stubVisualViewport(height);
  const hook = renderHook(() => useMobileKeyboard());
  const resizeTo = (h: number) =>
    act(() => {
      vp.setHeight(h);
      vp.fire("resize");
      drainRaf();
    });
  return { ...hook, vp, resizeTo };
}

describe("useMobileKeyboard", () => {
  it("reports mobile from an initial coarse pointer", () => {
    const { result } = mountMobile();
    expect(result.current).toEqual({ isMobile: true, keyboardOpen: false, keyboardHeight: 0 });
  });

  it("is a no-op on the desktop (fine pointer) path", () => {
    stubMatchMedia(false, "(pointer: coarse)");
    const ctl = stubVisualViewport(800);
    const { result } = renderHook(() => useMobileKeyboard());
    expect(result.current.isMobile).toBe(false);
    expect(ctl.listenerCount("resize") + ctl.listenerCount("scroll")).toBe(0);
  });

  it("measures the keyboard inset, ignores a URL-bar nudge, and closes on dismiss", () => {
    const { result, resizeTo } = mountMobile();
    resizeTo(760);
    expect(result.current).toMatchObject({ keyboardOpen: false, keyboardHeight: 0 });
    resizeTo(500);
    expect(result.current).toMatchObject({ keyboardOpen: true, keyboardHeight: 300 });
    resizeTo(800);
    expect(result.current).toMatchObject({ keyboardOpen: false, keyboardHeight: 0 });
  });

  it("starts polling when a text input gains focus", () => {
    const { result, vp } = mountMobile();
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
    const mq = stubMatchMedia(false, "(pointer: coarse)");
    stubVisualViewport(800);
    const { result } = renderHook(() => useMobileKeyboard());
    act(() => mq.set(true));
    expect(result.current.isMobile).toBe(true);
    act(() => mq.set(false));
    expect(result.current).toEqual({ isMobile: false, keyboardOpen: false, keyboardHeight: 0 });
  });

  it("removes all viewport listeners on unmount", () => {
    stubMatchMedia(true, "(pointer: coarse)");
    const docRemove = vi.spyOn(document, "removeEventListener");
    const winRemove = vi.spyOn(window, "removeEventListener");
    const { unmount, vp } = mountMobile();
    expect([vp.listenerCount("resize"), vp.listenerCount("scroll")]).toEqual([1, 1]);
    unmount();
    expect([vp.listenerCount("resize"), vp.listenerCount("scroll")]).toEqual([0, 0]);
    expect(docRemove).toHaveBeenCalledWith("focusin", expect.any(Function));
    expect(winRemove).toHaveBeenCalledWith("orientationchange", expect.any(Function));
    expect(winRemove).toHaveBeenCalledWith("scroll", expect.any(Function));
  });
});

// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useMobileViewportLock } from "./useMobileViewportLock";

beforeEach(() => {
  window.matchMedia = vi.fn().mockReturnValue({ matches: true }) as unknown as typeof window.matchMedia;
  window.scrollTo = vi.fn();
  vi.spyOn(window, "requestAnimationFrame").mockImplementation((cb) => {
    cb(performance.now());
    return 1;
  });
});

afterEach(() => vi.restoreAllMocks());

describe("useMobileViewportLock", () => {
  it("returns document scrolling to the origin on touch devices", () => {
    Object.defineProperty(window, "scrollY", { configurable: true, value: 120 });
    renderHook(() => useMobileViewportLock());
    expect(window.scrollTo).toHaveBeenCalledWith(0, 0);

    vi.mocked(window.scrollTo).mockClear();
    act(() => window.dispatchEvent(new Event("scroll")));
    expect(window.scrollTo).toHaveBeenCalledWith(0, 0);
  });

  it("does not interfere with desktop document scrolling", () => {
    window.matchMedia = vi.fn().mockReturnValue({ matches: false }) as unknown as typeof window.matchMedia;
    Object.defineProperty(window, "scrollY", { configurable: true, value: 120 });
    renderHook(() => useMobileViewportLock());
    act(() => window.dispatchEvent(new Event("scroll")));
    expect(window.scrollTo).not.toHaveBeenCalled();
  });
});

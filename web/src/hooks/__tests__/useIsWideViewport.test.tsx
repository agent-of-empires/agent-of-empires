// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";

import { useIsWideViewport } from "../useIsWideViewport";

type Listener = () => void;

function stubMatchMedia(initialMatches: boolean) {
  let matches = initialMatches;
  const listeners = new Set<Listener>();
  const mql = {
    get matches() {
      return matches;
    },
    media: "(min-width: 768px)",
    addEventListener: (_: string, cb: Listener) => listeners.add(cb),
    removeEventListener: (_: string, cb: Listener) => listeners.delete(cb),
  };
  window.matchMedia = vi
    .fn()
    .mockReturnValue(mql) as unknown as typeof window.matchMedia;
  return {
    set(next: boolean) {
      matches = next;
      listeners.forEach((cb) => cb());
    },
    listenerCount: () => listeners.size,
  };
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("useIsWideViewport", () => {
  it("reflects the initial match state", () => {
    stubMatchMedia(true);
    const { result } = renderHook(() => useIsWideViewport());
    expect(result.current).toBe(true);
  });

  it("starts false below the breakpoint", () => {
    stubMatchMedia(false);
    const { result } = renderHook(() => useIsWideViewport());
    expect(result.current).toBe(false);
  });

  it("updates when the media query changes and cleans up on unmount", () => {
    const ctl = stubMatchMedia(false);
    const { result, unmount } = renderHook(() => useIsWideViewport());
    expect(result.current).toBe(false);

    act(() => ctl.set(true));
    expect(result.current).toBe(true);

    expect(ctl.listenerCount()).toBe(1);
    unmount();
    expect(ctl.listenerCount()).toBe(0);
  });
});

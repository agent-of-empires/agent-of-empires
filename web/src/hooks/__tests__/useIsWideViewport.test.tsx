// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";

import { useIsWideViewport } from "../useIsWideViewport";
import { stubMatchMedia } from "./fixtures";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("useIsWideViewport", () => {
  it("reflects the initial match state", () => {
    stubMatchMedia(true, "(min-width: 768px)");
    const { result } = renderHook(() => useIsWideViewport());
    expect(result.current).toBe(true);
  });

  it("updates when the media query changes and cleans up on unmount", () => {
    const ctl = stubMatchMedia(false, "(min-width: 768px)");
    const { result, unmount } = renderHook(() => useIsWideViewport());
    expect(result.current).toBe(false);

    act(() => ctl.set(true));
    expect(result.current).toBe(true);

    expect(ctl.listenerCount()).toBe(1);
    unmount();
    expect(ctl.listenerCount()).toBe(0);
  });

  it("updates on a viewport resize when the browser skips the media-query event", () => {
    const ctl = stubMatchMedia(false, "(min-width: 768px)");
    const { result } = renderHook(() => useIsWideViewport());

    ctl.setWithoutEvent(true);
    act(() => window.dispatchEvent(new Event("resize")));

    expect(result.current).toBe(true);
  });
});

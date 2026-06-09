// @vitest-environment jsdom
//
// Contract test for the useSidebarSortMode hook (#1418). The hook is a
// thin React wrapper around sidebarSort's load/save helpers; the
// helpers' edge cases are covered by sidebarSort.test.ts. These tests
// pin the public hook contract: defaults to "manual" when localStorage
// is empty, restores a stored "lastActivity", and the setter persists.

import { renderHook, act } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";

import { useSidebarSortMode } from "./useSidebarSortMode";
import { SIDEBAR_SORT_MODE_KEY } from "../lib/sidebarSort";

beforeEach(() => {
  window.localStorage.clear();
});

describe("useSidebarSortMode", () => {
  it("defaults to 'manual' when localStorage is empty", () => {
    const { result } = renderHook(() => useSidebarSortMode());
    expect(result.current[0]).toBe("manual");
  });

  it("hydrates from a stored 'lastActivity' value", () => {
    window.localStorage.setItem(SIDEBAR_SORT_MODE_KEY, "lastActivity");
    const { result } = renderHook(() => useSidebarSortMode());
    expect(result.current[0]).toBe("lastActivity");
  });

  it("falls back to 'manual' for an unrecognised stored value", () => {
    window.localStorage.setItem(SIDEBAR_SORT_MODE_KEY, "garbage");
    const { result } = renderHook(() => useSidebarSortMode());
    expect(result.current[0]).toBe("manual");
  });

  it("setter updates state and persists to localStorage", () => {
    const { result } = renderHook(() => useSidebarSortMode());
    expect(result.current[0]).toBe("manual");

    act(() => {
      result.current[1]("lastActivity");
    });

    expect(result.current[0]).toBe("lastActivity");
    expect(window.localStorage.getItem(SIDEBAR_SORT_MODE_KEY)).toBe("lastActivity");
  });

  it("setter is stable across renders (useCallback)", () => {
    const { result, rerender } = renderHook(() => useSidebarSortMode());
    const setter1 = result.current[1];
    rerender();
    const setter2 = result.current[1];
    expect(setter1).toBe(setter2);
  });

  it("toggle back to 'manual' persists", () => {
    window.localStorage.setItem(SIDEBAR_SORT_MODE_KEY, "lastActivity");
    const { result } = renderHook(() => useSidebarSortMode());
    expect(result.current[0]).toBe("lastActivity");

    act(() => {
      result.current[1]("manual");
    });

    expect(result.current[0]).toBe("manual");
    expect(window.localStorage.getItem(SIDEBAR_SORT_MODE_KEY)).toBe("manual");
  });
});

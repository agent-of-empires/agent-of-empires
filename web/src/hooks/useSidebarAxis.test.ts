// @vitest-environment jsdom
//
// Contract test for the useSidebarAxis hook (#1234). The hook is a thin
// React wrapper around sidebarAxis's load/save helpers. These tests pin
// the public contract: defaults to "repo" when localStorage is empty,
// restores a stored "group", and the setter persists.

import { renderHook, act } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";

import { useSidebarAxis } from "./useSidebarAxis";
import { SIDEBAR_AXIS_KEY } from "../lib/sidebarAxis";

beforeEach(() => {
  window.localStorage.clear();
});

describe("useSidebarAxis", () => {
  it("defaults to 'repo' when localStorage is empty", () => {
    const { result } = renderHook(() => useSidebarAxis());
    expect(result.current[0]).toBe("repo");
  });

  it("hydrates from a stored 'group' value", () => {
    window.localStorage.setItem(SIDEBAR_AXIS_KEY, "group");
    const { result } = renderHook(() => useSidebarAxis());
    expect(result.current[0]).toBe("group");
  });

  it("hydrates from a stored 'repo+group' value (#1720)", () => {
    window.localStorage.setItem(SIDEBAR_AXIS_KEY, "repo+group");
    const { result } = renderHook(() => useSidebarAxis());
    expect(result.current[0]).toBe("repo+group");
  });

  it("falls back to 'repo' for an unrecognised stored value", () => {
    window.localStorage.setItem(SIDEBAR_AXIS_KEY, "garbage");
    const { result } = renderHook(() => useSidebarAxis());
    expect(result.current[0]).toBe("repo");
  });

  it("setter updates state and persists to localStorage", () => {
    const { result } = renderHook(() => useSidebarAxis());

    act(() => {
      result.current[1]("group");
    });

    expect(result.current[0]).toBe("group");
    expect(window.localStorage.getItem(SIDEBAR_AXIS_KEY)).toBe("group");
  });

  it("setter persists the 'repo+group' axis (#1720)", () => {
    const { result } = renderHook(() => useSidebarAxis());

    act(() => {
      result.current[1]("repo+group");
    });

    expect(result.current[0]).toBe("repo+group");
    expect(window.localStorage.getItem(SIDEBAR_AXIS_KEY)).toBe("repo+group");
  });

  it("setter is stable across renders (useCallback)", () => {
    const { result, rerender } = renderHook(() => useSidebarAxis());
    const setter1 = result.current[1];
    rerender();
    expect(result.current[1]).toBe(setter1);
  });
});

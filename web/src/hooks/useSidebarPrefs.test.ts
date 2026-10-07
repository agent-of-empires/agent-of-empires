// @vitest-environment jsdom

import { renderHook, act } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";

import { useSidebarAxis, useSidebarSortMode } from "./useSidebarPrefs";
import { SIDEBAR_AXIS_KEY } from "../lib/sidebarAxis";
import { SIDEBAR_SORT_MODE_KEY } from "../lib/sidebarSort";

beforeEach(() => {
  window.localStorage.clear();
});

function persistedChoiceCases<T extends string>(
  key: string,
  useChoice: () => readonly [T, (next: T) => void],
  fallback: T,
  stored: readonly T[],
) {
  it("falls back when nothing is stored or the stored value is unrecognised", () => {
    expect(renderHook(() => useChoice()).result.current[0]).toBe(fallback);
    window.localStorage.setItem(key, "garbage");
    expect(renderHook(() => useChoice()).result.current[0]).toBe(fallback);
  });

  it.each(stored)("hydrates a stored '%s'", (value) => {
    window.localStorage.setItem(key, value);
    expect(renderHook(() => useChoice()).result.current[0]).toBe(value);
  });

  it("persists each value the setter is given and keeps the setter stable", () => {
    const { result, rerender } = renderHook(() => useChoice());
    const setter = result.current[1];
    for (const value of [...stored, fallback]) {
      act(() => result.current[1](value));
      expect(result.current[0]).toBe(value);
      expect(window.localStorage.getItem(key)).toBe(value);
    }
    rerender();
    expect(result.current[1]).toBe(setter);
  });
}

// "org" (#3283) and "repo+group" (#1720) must survive a reload.
describe("useSidebarAxis", () =>
  persistedChoiceCases(SIDEBAR_AXIS_KEY, useSidebarAxis, "repo", ["org", "group", "repo+group"]));

describe("useSidebarSortMode", () =>
  persistedChoiceCases(SIDEBAR_SORT_MODE_KEY, useSidebarSortMode, "manual", ["lastActivity"]));

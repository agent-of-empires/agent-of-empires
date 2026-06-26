// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { usePaneLayout } from "../paneLayout";

beforeEach(() => localStorage.clear());
afterEach(() => localStorage.clear());

describe("usePaneLayout", () => {
  it("migrates the legacy collapsed flag (1 = both closed)", () => {
    localStorage.setItem("aoe-right-collapsed", "1");
    const { result } = renderHook(() => usePaneLayout());
    expect(result.current.layout).toEqual({ diff: false, terminal: false });
  });

  it("migrates the legacy expanded flag (0 = both open)", () => {
    localStorage.setItem("aoe-right-collapsed", "0");
    const { result } = renderHook(() => usePaneLayout());
    expect(result.current.layout).toEqual({ diff: true, terminal: true });
  });

  it("reads back persisted per-pane state and ignores malformed JSON", () => {
    localStorage.setItem("aoe-pane-layout", JSON.stringify({ diff: false, terminal: true }));
    expect(renderHook(() => usePaneLayout()).result.current.layout).toEqual({ diff: false, terminal: true });

    localStorage.setItem("aoe-pane-layout", "{not json");
    // Falls through to defaults rather than throwing; both keys agree on shape.
    expect(renderHook(() => usePaneLayout()).result.current.layout).toHaveProperty("diff");
  });

  it("togglePane flips one pane and persists", () => {
    localStorage.setItem("aoe-pane-layout", JSON.stringify({ diff: true, terminal: true }));
    const { result } = renderHook(() => usePaneLayout());
    act(() => result.current.togglePane("diff"));
    expect(result.current.layout).toEqual({ diff: false, terminal: true });
    expect(JSON.parse(localStorage.getItem("aoe-pane-layout")!)).toEqual({ diff: false, terminal: true });
  });
});

// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { openPanesAt, usePaneLayout } from "../paneLayout";

const RIGHT = { open: true, dock: "right" as const };
const CLOSED = { open: false, dock: "right" as const };

beforeEach(() => localStorage.clear());
afterEach(() => localStorage.clear());

describe("usePaneLayout", () => {
  it("migrates the legacy collapsed flag (1 = both closed)", () => {
    localStorage.setItem("aoe-right-collapsed", "1");
    const { result } = renderHook(() => usePaneLayout());
    expect(result.current.layout).toEqual({ diff: CLOSED, terminal: CLOSED });
  });

  it("migrates the legacy expanded flag (0 = both open)", () => {
    localStorage.setItem("aoe-right-collapsed", "0");
    const { result } = renderHook(() => usePaneLayout());
    expect(result.current.layout).toEqual({ diff: RIGHT, terminal: RIGHT });
  });

  it("migrates a phase-1 boolean layout to the dock shape", () => {
    localStorage.setItem("aoe-pane-layout", JSON.stringify({ diff: false, terminal: true }));
    expect(renderHook(() => usePaneLayout()).result.current.layout).toEqual({ diff: CLOSED, terminal: RIGHT });
  });

  it("reads back persisted per-pane dock state and ignores malformed JSON", () => {
    localStorage.setItem(
      "aoe-pane-layout",
      JSON.stringify({ diff: { open: true, dock: "right" }, terminal: { open: true, dock: "bottom" } }),
    );
    const { result } = renderHook(() => usePaneLayout());
    expect(result.current.layout.terminal).toEqual({ open: true, dock: "bottom" });

    localStorage.setItem("aoe-pane-layout", "{not json");
    expect(renderHook(() => usePaneLayout()).result.current.layout).toHaveProperty("diff");
  });

  it("togglePane flips one pane and persists", () => {
    localStorage.setItem("aoe-pane-layout", JSON.stringify({ diff: RIGHT, terminal: RIGHT }));
    const { result } = renderHook(() => usePaneLayout());
    act(() => result.current.togglePane("diff"));
    expect(result.current.layout.diff.open).toBe(false);
    expect(JSON.parse(localStorage.getItem("aoe-pane-layout")!).diff.open).toBe(false);
  });

  it("movePane reassigns a pane's dock; openPanesAt groups by location", () => {
    localStorage.setItem("aoe-pane-layout", JSON.stringify({ diff: RIGHT, terminal: RIGHT }));
    const { result } = renderHook(() => usePaneLayout());
    act(() => result.current.movePane("terminal", "bottom"));
    expect(openPanesAt(result.current.layout, "right")).toEqual(["diff"]);
    expect(openPanesAt(result.current.layout, "bottom")).toEqual(["terminal"]);
  });
});

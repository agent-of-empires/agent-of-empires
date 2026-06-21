// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";
import { useRef, type Dispatch, type SetStateAction } from "react";
import { clampMenuPosition, useClampedMenuPosition } from "../menuPosition";

describe("clampMenuPosition", () => {
  const VW = 1000;
  const VH = 800;

  it("returns the anchor unchanged when the menu fits at the cursor", () => {
    const out = clampMenuPosition({
      x: 100,
      y: 100,
      menuWidth: 200,
      menuHeight: 300,
      viewportWidth: VW,
      viewportHeight: VH,
    });
    expect(out).toEqual({ x: 100, y: 100 });
  });

  it("clamps top when the menu would overflow the bottom edge", () => {
    const out = clampMenuPosition({
      x: 100,
      y: 750,
      menuWidth: 200,
      menuHeight: 300,
      viewportWidth: VW,
      viewportHeight: VH,
    });
    expect(out.y).toBe(VH - 300 - 8);
    expect(out.x).toBe(100);
  });

  it("clamps left when the menu would overflow the right edge", () => {
    const out = clampMenuPosition({
      x: 950,
      y: 100,
      menuWidth: 200,
      menuHeight: 300,
      viewportWidth: VW,
      viewportHeight: VH,
    });
    expect(out.x).toBe(VW - 200 - 8);
    expect(out.y).toBe(100);
  });

  it("clamps both axes when the menu would overflow the bottom-right corner", () => {
    const out = clampMenuPosition({
      x: 950,
      y: 750,
      menuWidth: 200,
      menuHeight: 300,
      viewportWidth: VW,
      viewportHeight: VH,
    });
    expect(out).toEqual({ x: VW - 200 - 8, y: VH - 300 - 8 });
  });

  it("collapses to the margin when the menu is taller than the viewport", () => {
    const out = clampMenuPosition({
      x: 50,
      y: 50,
      menuWidth: 200,
      menuHeight: 5000,
      viewportWidth: VW,
      viewportHeight: VH,
    });
    expect(out.y).toBe(8);
  });

  it("honors a custom margin", () => {
    const out = clampMenuPosition({
      x: 950,
      y: 100,
      menuWidth: 200,
      menuHeight: 100,
      viewportWidth: VW,
      viewportHeight: VH,
      margin: 16,
    });
    expect(out.x).toBe(VW - 200 - 16);
  });

  it("clamps negative anchors up to the margin", () => {
    const out = clampMenuPosition({
      x: -50,
      y: -30,
      menuWidth: 200,
      menuHeight: 300,
      viewportWidth: VW,
      viewportHeight: VH,
    });
    expect(out).toEqual({ x: 8, y: 8 });
  });
});

describe("useClampedMenuPosition", () => {
  function setupHook(opts: {
    anchor: { x: number; y: number } | null;
    menuRect: { width: number; height: number };
    viewport: { width: number; height: number };
  }) {
    // The hook forwards a functional updater so callers carrying extra menu
    // state keep it through a reposition. See #2312.
    const setContextMenu = vi.fn<Dispatch<SetStateAction<{ x: number; y: number } | null>>>();
    const menu = document.createElement("div");
    menu.getBoundingClientRect = () =>
      ({
        width: opts.menuRect.width,
        height: opts.menuRect.height,
        x: 0,
        y: 0,
        top: 0,
        left: 0,
        right: opts.menuRect.width,
        bottom: opts.menuRect.height,
        toJSON: () => ({}),
      }) as DOMRect;
    Object.defineProperty(window, "innerWidth", {
      configurable: true,
      value: opts.viewport.width,
    });
    Object.defineProperty(window, "innerHeight", {
      configurable: true,
      value: opts.viewport.height,
    });
    const { rerender } = renderHook(
      ({ ctx }: { ctx: { x: number; y: number } | null }) => {
        const ref = useRef<HTMLDivElement | null>(menu);
        useClampedMenuPosition(ctx, ref, setContextMenu);
      },
      { initialProps: { ctx: opts.anchor } },
    );
    return { setContextMenu, rerender };
  }

  it("no-ops when the menu fits at the anchor", () => {
    const { setContextMenu } = setupHook({
      anchor: { x: 100, y: 100 },
      menuRect: { width: 180, height: 240 },
      viewport: { width: 1280, height: 720 },
    });
    expect(setContextMenu).not.toHaveBeenCalled();
  });

  it("flips the menu upward when the anchor overflows the bottom edge", () => {
    const { setContextMenu } = setupHook({
      anchor: { x: 100, y: 700 },
      menuRect: { width: 180, height: 240 },
      viewport: { width: 1280, height: 720 },
    });
    const updater = setContextMenu.mock.calls[0][0] as (
      prev: { x: number; y: number } | null,
    ) => { x: number; y: number } | null;
    expect(updater({ x: 100, y: 700 })).toEqual({ x: 100, y: 472 });
    // Extra menu state on the previous value survives the reposition.
    expect(updater({ x: 100, y: 700, scope: "bulk" } as never)).toEqual({ x: 100, y: 472, scope: "bulk" });
  });

  it("clamps the menu left when the anchor overflows the right edge", () => {
    const { setContextMenu } = setupHook({
      anchor: { x: 1270, y: 100 },
      menuRect: { width: 180, height: 240 },
      viewport: { width: 1280, height: 720 },
    });
    const updater = setContextMenu.mock.calls[0][0] as (
      prev: { x: number; y: number } | null,
    ) => { x: number; y: number } | null;
    expect(updater({ x: 1270, y: 100 })).toEqual({ x: 1092, y: 100 });
  });

  it("does not call setContextMenu when contextMenu is null", () => {
    const { setContextMenu } = setupHook({
      anchor: null,
      menuRect: { width: 180, height: 240 },
      viewport: { width: 1280, height: 720 },
    });
    expect(setContextMenu).not.toHaveBeenCalled();
  });
});

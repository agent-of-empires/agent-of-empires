// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";
import { useRef, type Dispatch, type SetStateAction } from "react";
import { clampMenuPosition, useClampedMenuPosition } from "../menuPosition";

describe("clampMenuPosition", () => {
  it.each<[string, number, number, number, number, number | undefined, { x: number; y: number }]>([
    ["fits at the cursor", 100, 100, 200, 300, undefined, { x: 100, y: 100 }],
    ["overflows the bottom", 100, 750, 200, 300, undefined, { x: 100, y: 800 - 300 - 8 }],
    ["overflows the right", 950, 100, 200, 300, undefined, { x: 1000 - 200 - 8, y: 100 }],
    ["overflows the corner", 950, 750, 200, 300, undefined, { x: 1000 - 200 - 8, y: 800 - 300 - 8 }],
    ["is taller than the viewport", 50, 50, 200, 5000, undefined, { x: 50, y: 8 }],
    ["uses a custom margin", 950, 100, 200, 100, 16, { x: 1000 - 200 - 16, y: 100 }],
    ["has negative anchors", -50, -30, 200, 300, undefined, { x: 8, y: 8 }],
  ])("menu that %s", (_name, x, y, menuWidth, menuHeight, margin, expected) => {
    expect(
      clampMenuPosition({ x, y, menuWidth, menuHeight, viewportWidth: 1000, viewportHeight: 800, margin }),
    ).toEqual(expected);
  });
});

describe("useClampedMenuPosition", () => {
  function setupHook(opts: {
    anchor: { x: number; y: number } | null;
    menuRect: { width: number; height: number };
    viewport: { width: number; height: number };
  }) {
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
    expect(updater({ x: 100, y: 700, scope: "bulk" } as never)).toEqual({ x: 100, y: 472, scope: "bulk" });
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

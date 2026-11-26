// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { useDrawerDrag } from "../sidebar/useDrawerDrag";

const el = { getBoundingClientRect: () => ({ width: 300 }) };
const ev = (x: number, y: number, pointerType = "touch") =>
  ({ pointerId: 1, clientX: x, clientY: y, pointerType, currentTarget: el }) as never;

function drag(points: [number, number][], pointerType = "touch") {
  const { result } = renderHook(() => useDrawerDrag({ open: true, side: "left" }));
  const [first, ...rest] = points;
  act(() => result.current.handlers.onPointerDown(ev(...first!, pointerType)));
  const seen: [number | null, number][] = [];
  for (const p of rest) {
    act(() => result.current.handlers.onPointerMove(ev(...p)));
    seen.push([result.current.offset, Math.round(result.current.progress * 100)]);
  }
  act(() => result.current.handlers.onPointerUp(ev(...rest.at(-1)!)));
  return { seen, result };
}

describe("useDrawerDrag", () => {
  it("follows a drag toward the edge, fading the backdrop, and lets go on release", () => {
    const { seen, result } = drag([
      [250, 300],
      [230, 302],
      [100, 305],
    ]);
    expect([seen, result.current.offset, result.current.progress]).toEqual([
      [
        [-20, 93],
        [-150, 50],
      ],
      null,
      1,
    ]);
  });

  it("leaves vertical scrolls, drags away from the edge and the mouse alone", () => {
    for (const [points, pointerType] of [
      [
        [
          [250, 300],
          [252, 360],
          [180, 380],
        ],
        "touch",
      ],
      [
        [
          [100, 300],
          [160, 300],
          [260, 300],
        ],
        "touch",
      ],
      [
        [
          [250, 300],
          [150, 300],
          [40, 300],
        ],
        "mouse",
      ],
    ] as const) {
      const { seen } = drag(points as never, pointerType);
      expect(seen.every(([offset]) => offset === null)).toBe(true);
    }
  });

  it("swallows the click that ends a drag", () => {
    const { result } = drag([
      [250, 300],
      [200, 300],
    ]);
    const click = { preventDefault: vi.fn(), stopPropagation: vi.fn() };
    result.current.handlers.onClickCapture(click as never);
    expect(click.stopPropagation).toHaveBeenCalled();
  });
});

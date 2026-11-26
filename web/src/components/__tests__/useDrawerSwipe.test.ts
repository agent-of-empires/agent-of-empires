// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { useDrawerSwipe } from "../sidebar/useDrawerSwipe";

const el = { getBoundingClientRect: () => ({ width: 300 }), setPointerCapture: vi.fn() };
const ev = (x: number, y: number, t: number, pointerType = "touch") =>
  ({ pointerId: 1, clientX: x, clientY: y, timeStamp: t, pointerType, currentTarget: el }) as never;

function drag(points: [number, number, number][], opts: { pointerType?: string } = {}) {
  const onClose = vi.fn();
  const { result } = renderHook(() => useDrawerSwipe({ open: true, side: "left", onClose }));
  const [first, ...rest] = points;
  act(() => result.current.handlers.onPointerDown(ev(...first!, opts.pointerType)));
  const offsets: (number | null)[] = [];
  for (const p of rest) {
    act(() => result.current.handlers.onPointerMove(ev(...p)));
    offsets.push(result.current.offset);
  }
  act(() => result.current.handlers.onPointerUp(ev(...rest.at(-1)!)));
  return { onClose, offsets, result };
}

describe("useDrawerSwipe", () => {
  it("follows a leftward drag and closes past a third of the width", () => {
    const { onClose, offsets, result } = drag([
      [250, 300, 0],
      [230, 302, 100],
      [140, 305, 400],
    ]);
    expect([offsets, onClose.mock.calls.length, result.current.offset]).toEqual([[-20, -110], 1, null]);
  });

  it("springs back from a short, slow drag", () => {
    const { onClose } = drag([
      [250, 300, 0],
      [230, 300, 200],
      [200, 300, 600],
    ]);
    expect(onClose).not.toHaveBeenCalled();
  });

  it("closes on a short flick", () => {
    const { onClose } = drag([
      [250, 300, 0],
      [235, 300, 10],
      [200, 300, 60],
    ]);
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("leaves vertical scrolls, rightward drags and the mouse alone", () => {
    for (const [points, opts] of [
      [
        [
          [250, 300, 0],
          [252, 360, 100],
          [180, 380, 200],
        ],
        {},
      ],
      [
        [
          [100, 300, 0],
          [160, 300, 100],
          [260, 300, 200],
        ],
        {},
      ],
      [
        [
          [250, 300, 0],
          [150, 300, 100],
          [40, 300, 200],
        ],
        { pointerType: "mouse" },
      ],
    ] as const) {
      const { onClose, offsets } = drag(points as never, opts);
      expect([onClose.mock.calls.length, offsets.every((o) => o === null)]).toEqual([0, true]);
    }
  });

  it("swallows the click that ends a drag", () => {
    const { result } = drag([
      [250, 300, 0],
      [200, 300, 100],
      [150, 300, 300],
    ]);
    const click = { preventDefault: vi.fn(), stopPropagation: vi.fn() };
    result.current.handlers.onClickCapture(click as never);
    expect(click.stopPropagation).toHaveBeenCalled();
  });
});

// @vitest-environment jsdom

import { renderHook, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useLongPressDrag, type DragAxis } from "./useLongPressDrag";
import type { PointerEvent as ReactPointerEvent } from "react";

const ptr = (clientX: number, clientY: number) => ({ clientX, clientY }) as ReactPointerEvent;

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

function mount() {
  const onRepeat = vi.fn();
  const onHorizontal = vi.fn();
  const onAxisChange = vi.fn<(axis: DragAxis) => void>();
  const hook = renderHook(() => useLongPressDrag({ onRepeat, onHorizontal, onAxisChange }));
  const h = () => hook.result.current;
  const step = (fn: () => void) => act(fn);
  const advance = (ms: number) => act(() => void vi.advanceTimersByTime(ms));
  return { onRepeat, onHorizontal, onAxisChange, h, step, advance, unmount: hook.unmount };
}

describe("useLongPressDrag", () => {
  it("taps once on a short vertical press", () => {
    const { onRepeat, onHorizontal, h, step, advance } = mount();
    step(() => h().onPointerDown(ptr(10, 10)));
    advance(100);
    step(() => h().onPointerUp(ptr(10, 10)));
    expect(onRepeat).toHaveBeenCalledTimes(1);
    expect(onHorizontal).not.toHaveBeenCalled();
  });

  it.each<[string, (m: ReturnType<typeof mount>) => void]>([
    [
      "a horizontal drag changed the axis",
      ({ h }) => {
        h().onPointerDown(ptr(10, 10));
        h().onPointerMove(ptr(40, 12));
        h().onPointerUp(ptr(40, 12));
      },
    ],
    [
      "the press was cancelled",
      ({ h }) => {
        h().onPointerDown(ptr(10, 10));
        h().onPointerCancel(ptr(10, 10));
        h().onPointerUp(ptr(10, 10));
      },
    ],
  ])("does not tap when %s", (_label, run) => {
    const m = mount();
    m.step(() => run(m));
    expect(m.onRepeat).not.toHaveBeenCalled();
    expect(m.onHorizontal).not.toHaveBeenCalled();
  });

  it("repeats every 100ms after a 300ms hold and stops on release", () => {
    const { onRepeat, h, step, advance } = mount();
    step(() => h().onPointerDown(ptr(10, 10)));
    advance(300);
    expect(onRepeat).not.toHaveBeenCalled();
    advance(300);
    expect(onRepeat).toHaveBeenCalledTimes(3);
    step(() => h().onPointerUp(ptr(10, 10)));
    advance(500);
    expect(onRepeat).toHaveBeenCalledTimes(3);
  });

  it("repeats horizontal arrows once the axis is horizontal", () => {
    const { onRepeat, onHorizontal, h, step, advance } = mount();
    step(() => {
      h().onPointerDown(ptr(10, 10));
      h().onPointerMove(ptr(-20, 12));
    });
    advance(500);
    expect(onHorizontal).toHaveBeenCalledTimes(2);
    expect(onHorizontal).toHaveBeenLastCalledWith("left");
    expect(onRepeat).not.toHaveBeenCalled();
  });

  it("reports the dominant axis past the 16px threshold, and ignores moves before a press", () => {
    const { onAxisChange, h, step } = mount();
    step(() => h().onPointerMove(ptr(100, 100)));
    expect(onAxisChange).not.toHaveBeenCalled();

    const seen: [number, number, DragAxis][] = [
      [20, 11, "vertical"],
      [40, 12, "horizontal-right"],
      [11, 60, "vertical"],
    ];
    step(() => h().onPointerDown(ptr(10, 10)));
    for (const [x, y, axis] of seen) {
      step(() => h().onPointerMove(ptr(x, y)));
      expect(onAxisChange).toHaveBeenLastCalledWith(axis);
    }
    expect(onAxisChange.mock.calls.map(([a]) => a)).toEqual(["vertical", "horizontal-right", "vertical"]);
  });

  it.each<[string, (m: ReturnType<typeof mount>) => void, number]>([
    ["pointer cancel after one repeat", ({ h, step }) => step(() => h().onPointerCancel(ptr(10, 10))), 1],
    ["unmount", ({ unmount }) => unmount(), 1],
  ])("%s stops the repeat", (_label, stop, calls) => {
    const m = mount();
    m.step(() => m.h().onPointerDown(ptr(10, 10)));
    m.advance(400);
    stop(m);
    m.advance(1000);
    expect(m.onRepeat).toHaveBeenCalledTimes(calls);
  });

  it("pointer cancel resets the axis to vertical; leave aborts a pending hold", () => {
    const m = mount();
    m.step(() => {
      m.h().onPointerDown(ptr(10, 10));
      m.h().onPointerMove(ptr(40, 12));
      m.h().onPointerCancel(ptr(40, 12));
    });
    expect(m.onAxisChange).toHaveBeenLastCalledWith("vertical");

    m.step(() => m.h().onPointerDown(ptr(10, 10)));
    m.advance(100);
    m.step(() => m.h().onPointerLeave(ptr(10, 10)));
    m.advance(1000);
    expect(m.onRepeat).not.toHaveBeenCalled();
  });
});

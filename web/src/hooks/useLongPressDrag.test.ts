// @vitest-environment jsdom
//
// Hook tests for useLongPressDrag. The hook returns pointer handlers that
// implement: tap-to-repeat on release (short press, vertical, no emit),
// press-and-hold to repeat the active axis every 100ms after a 300ms delay,
// horizontal drag past 16px to switch the emit axis (dominant axis wins),
// and an axis-change callback for the visual hint. cancel/leave abort.
//
// The handlers take React pointer events but only read clientX/clientY, so
// plain objects suffice. Timers are faked to drive the 300ms long-press
// delay and 100ms repeat interval deterministically.

import { renderHook, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useLongPressDrag, type DragAxis } from "./useLongPressDrag";
import type { PointerEvent as ReactPointerEvent } from "react";

function ptr(clientX: number, clientY: number): ReactPointerEvent {
  return { clientX, clientY } as ReactPointerEvent;
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useLongPressDrag tap", () => {
  it("fires a single onRepeat on a short vertical press + release", () => {
    const onRepeat = vi.fn();
    const onHorizontal = vi.fn();
    const { result } = renderHook(() => useLongPressDrag({ onRepeat, onHorizontal }));

    act(() => {
      result.current.onPointerDown(ptr(10, 10));
    });
    // Release before the 300ms long-press delay elapses.
    act(() => {
      vi.advanceTimersByTime(100);
      result.current.onPointerUp(ptr(10, 10));
    });

    expect(onRepeat).toHaveBeenCalledTimes(1);
    expect(onHorizontal).not.toHaveBeenCalled();
  });

  it("does not tap if a horizontal drag changed the axis before release", () => {
    const onRepeat = vi.fn();
    const onHorizontal = vi.fn();
    const { result } = renderHook(() => useLongPressDrag({ onRepeat, onHorizontal }));

    act(() => {
      result.current.onPointerDown(ptr(10, 10));
      result.current.onPointerMove(ptr(40, 12)); // dx 30 > 16, horizontal-right
      result.current.onPointerUp(ptr(40, 12));
    });

    expect(onRepeat).not.toHaveBeenCalled();
    expect(onHorizontal).not.toHaveBeenCalled();
  });
});

describe("useLongPressDrag press-and-hold repeat", () => {
  it("repeats onRepeat every 100ms after the 300ms delay while held vertical", () => {
    const onRepeat = vi.fn();
    const onHorizontal = vi.fn();
    const { result } = renderHook(() => useLongPressDrag({ onRepeat, onHorizontal }));

    act(() => {
      result.current.onPointerDown(ptr(10, 10));
    });
    // Nothing yet before the delay.
    act(() => {
      vi.advanceTimersByTime(300);
    });
    expect(onRepeat).not.toHaveBeenCalled();

    act(() => {
      vi.advanceTimersByTime(300); // 3 intervals
    });
    expect(onRepeat).toHaveBeenCalledTimes(3);

    // Release: long-press emitted, so no extra tap, and the interval stops.
    act(() => {
      result.current.onPointerUp(ptr(10, 10));
      vi.advanceTimersByTime(500);
    });
    expect(onRepeat).toHaveBeenCalledTimes(3);
  });

  it("emits horizontal arrows on repeat once the axis is horizontal", () => {
    const onRepeat = vi.fn();
    const onHorizontal = vi.fn();
    const { result } = renderHook(() => useLongPressDrag({ onRepeat, onHorizontal }));

    act(() => {
      result.current.onPointerDown(ptr(10, 10));
      result.current.onPointerMove(ptr(-20, 12)); // dx -30, horizontal-left
    });
    act(() => {
      vi.advanceTimersByTime(300); // delay
      vi.advanceTimersByTime(200); // 2 intervals
    });

    expect(onHorizontal).toHaveBeenCalledTimes(2);
    expect(onHorizontal).toHaveBeenLastCalledWith("left");
    expect(onRepeat).not.toHaveBeenCalled();
  });
});

describe("useLongPressDrag axis tracking", () => {
  it("reports axis changes via onAxisChange (dominant axis wins)", () => {
    const onRepeat = vi.fn();
    const onHorizontal = vi.fn();
    const onAxisChange = vi.fn<(axis: DragAxis) => void>();
    const { result } = renderHook(() => useLongPressDrag({ onRepeat, onHorizontal, onAxisChange }));

    act(() => {
      result.current.onPointerDown(ptr(10, 10)); // -> vertical
    });
    expect(onAxisChange).toHaveBeenLastCalledWith("vertical");

    act(() => {
      result.current.onPointerMove(ptr(40, 12)); // dx 30 dominant -> horizontal-right
    });
    expect(onAxisChange).toHaveBeenLastCalledWith("horizontal-right");

    act(() => {
      result.current.onPointerMove(ptr(11, 60)); // dy 50 dominant -> vertical
    });
    expect(onAxisChange).toHaveBeenLastCalledWith("vertical");
  });

  it("stays vertical when horizontal movement is under the 16px threshold", () => {
    const onRepeat = vi.fn();
    const onHorizontal = vi.fn();
    const onAxisChange = vi.fn<(axis: DragAxis) => void>();
    const { result } = renderHook(() => useLongPressDrag({ onRepeat, onHorizontal, onAxisChange }));

    act(() => {
      result.current.onPointerDown(ptr(10, 10));
      result.current.onPointerMove(ptr(20, 11)); // dx 10 < 16 -> still vertical
    });
    // Only the initial "vertical" from pointerDown; no axis change emitted.
    expect(onAxisChange).toHaveBeenCalledTimes(1);
    expect(onAxisChange).toHaveBeenCalledWith("vertical");
  });

  it("ignores pointer moves before a press (pressed=false)", () => {
    const onRepeat = vi.fn();
    const onHorizontal = vi.fn();
    const onAxisChange = vi.fn<(axis: DragAxis) => void>();
    const { result } = renderHook(() => useLongPressDrag({ onRepeat, onHorizontal, onAxisChange }));

    act(() => {
      result.current.onPointerMove(ptr(100, 100));
    });
    expect(onAxisChange).not.toHaveBeenCalled();
  });
});

describe("useLongPressDrag cancel / leave", () => {
  it("onPointerCancel stops the interval and resets to vertical", () => {
    const onRepeat = vi.fn();
    const onHorizontal = vi.fn();
    const onAxisChange = vi.fn<(axis: DragAxis) => void>();
    const { result } = renderHook(() => useLongPressDrag({ onRepeat, onHorizontal, onAxisChange }));

    act(() => {
      result.current.onPointerDown(ptr(10, 10));
      vi.advanceTimersByTime(400); // delay + one interval
    });
    expect(onRepeat).toHaveBeenCalledTimes(1);

    act(() => {
      result.current.onPointerCancel(ptr(10, 10));
    });
    expect(onAxisChange).toHaveBeenLastCalledWith("vertical");

    act(() => {
      vi.advanceTimersByTime(500);
    });
    // Interval cleared: no further repeats.
    expect(onRepeat).toHaveBeenCalledTimes(1);
  });

  it("onPointerLeave aborts a pending long-press before it starts repeating", () => {
    const onRepeat = vi.fn();
    const onHorizontal = vi.fn();
    const { result } = renderHook(() => useLongPressDrag({ onRepeat, onHorizontal }));

    act(() => {
      result.current.onPointerDown(ptr(10, 10));
      vi.advanceTimersByTime(100); // before the 300ms delay
      result.current.onPointerLeave(ptr(10, 10));
      vi.advanceTimersByTime(1000);
    });
    expect(onRepeat).not.toHaveBeenCalled();
  });

  it("a release after cancel does not produce a tap", () => {
    const onRepeat = vi.fn();
    const onHorizontal = vi.fn();
    const { result } = renderHook(() => useLongPressDrag({ onRepeat, onHorizontal }));

    act(() => {
      result.current.onPointerDown(ptr(10, 10));
      result.current.onPointerCancel(ptr(10, 10)); // pressed -> false
      result.current.onPointerUp(ptr(10, 10));
    });
    expect(onRepeat).not.toHaveBeenCalled();
  });
});

describe("useLongPressDrag cleanup", () => {
  it("clears timers on unmount", () => {
    const onRepeat = vi.fn();
    const onHorizontal = vi.fn();
    const { result, unmount } = renderHook(() => useLongPressDrag({ onRepeat, onHorizontal }));

    act(() => {
      result.current.onPointerDown(ptr(10, 10));
      vi.advanceTimersByTime(300); // arm the interval
    });
    unmount();
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    // Unmount cleanup cleared the interval; no repeats fire post-unmount.
    expect(onRepeat).toHaveBeenCalledTimes(0);
  });
});

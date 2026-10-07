// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { ArrowJoystick } from "../ArrowJoystick";
import {
  JOYSTICK_FAST_REPEAT_MS,
  JOYSTICK_FIRST_REPEAT_MS,
  JOYSTICK_FULL_SPEED_PX,
  JOYSTICK_SLOW_REPEAT_MS,
  joystickDirection,
  joystickRepeatMs,
} from "../../lib/arrowJoystick";

const RIGHT = "\x1b[C";
const UP = "\x1b[A";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

function mount() {
  vi.useFakeTimers();
  const onArrow = vi.fn<(sequence: string) => void>();
  render(<ArrowJoystick onArrow={onArrow} />);
  const pad = screen.getByRole("group", { name: "Arrow keys joystick" });
  const at = (dx: number, dy: number) => ({ pointerId: 1, clientX: 100 + dx, clientY: 100 + dy });
  return {
    onArrow,
    down: () => fireEvent.pointerDown(pad, at(0, 0)),
    move: (dx: number, dy: number) => fireEvent.pointerMove(pad, at(dx, dy)),
    up: () => fireEvent.pointerUp(pad, at(0, 0)),
    wait: (ms: number) => act(() => vi.advanceTimersByTime(ms)),
    sent: () => onArrow.mock.calls.map(([s]) => s),
  };
}

describe("joystick math", () => {
  it.each([
    [11, 0, null],
    [-11, 11, null],
    [12, 0, "right"],
    [-20, 5, "left"],
    [5, 20, "down"],
    [8, -30, "up"],
    // A tie goes vertical.
    [20, 20, "down"],
  ])("(%i, %i) points %s", (dx, dy, expected) => {
    expect(joystickDirection(dx, dy)).toBe(expected);
  });

  it("repeats faster with distance, clamped at both ends", () => {
    expect(joystickRepeatMs(0)).toBe(JOYSTICK_SLOW_REPEAT_MS);
    expect(joystickRepeatMs(JOYSTICK_FULL_SPEED_PX * 3)).toBe(JOYSTICK_FAST_REPEAT_MS);
    expect(joystickRepeatMs(36)).toBeLessThan(JOYSTICK_SLOW_REPEAT_MS);
    expect(joystickRepeatMs(36)).toBeGreaterThan(JOYSTICK_FAST_REPEAT_MS);
  });
});

describe("ArrowJoystick", () => {
  it("sends nothing for a tap or a wobble inside the dead zone", () => {
    const j = mount();
    j.down();
    j.move(6, -8);
    j.wait(2000);
    j.up();
    expect(j.onArrow).not.toHaveBeenCalled();
  });

  it("sends once past the dead zone, then repeats faster the further the drag", () => {
    const j = mount();
    j.down();
    j.move(14, 3);
    expect(j.sent()).toEqual([RIGHT]);
    j.wait(JOYSTICK_FIRST_REPEAT_MS - 1);
    expect(j.sent()).toHaveLength(1);
    j.wait(1);
    expect(j.sent()).toHaveLength(2);
    const near = joystickRepeatMs(14);
    j.wait(near);
    expect(j.sent()).toHaveLength(3);

    // Same direction, further out: no extra send, and the next interval uses the new distance.
    j.move(JOYSTICK_FULL_SPEED_PX + 20, 0);
    expect(j.sent()).toHaveLength(3);
    j.wait(near);
    j.wait(5 * JOYSTICK_FAST_REPEAT_MS);
    expect(j.sent()).toEqual(Array(9).fill(RIGHT));
  });

  it("switches axis at once, pauses in the dead zone, and stops on release", () => {
    const j = mount();
    j.down();
    j.move(20, 0);
    j.move(4, -25);
    expect(j.sent()).toEqual([RIGHT, UP]);
    j.wait(JOYSTICK_FIRST_REPEAT_MS);
    expect(j.sent()).toEqual([RIGHT, UP, UP]);

    j.move(2, 2);
    j.wait(2000);
    expect(j.sent()).toHaveLength(3);

    j.move(0, -30);
    expect(j.sent()).toHaveLength(4);
    j.up();
    j.wait(2000);
    expect(j.sent()).toHaveLength(4);
  });

  it("keeps its touches from reaching window listeners such as the sidebar edge swipe", () => {
    const j = mount();
    const onWindowTouch = vi.fn();
    window.addEventListener("touchstart", onWindowTouch);
    window.addEventListener("touchmove", onWindowTouch);
    const pad = screen.getByRole("group", { name: "Arrow keys joystick" });
    fireEvent.touchStart(pad);
    fireEvent.touchMove(pad);
    expect(onWindowTouch).not.toHaveBeenCalled();
    window.removeEventListener("touchstart", onWindowTouch);
    window.removeEventListener("touchmove", onWindowTouch);
    expect(j.onArrow).not.toHaveBeenCalled();
  });
});

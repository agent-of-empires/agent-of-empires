import { describe, expect, it } from "vitest";
import { exceedsTouchSlop, LONG_PRESS_SLOP_PX } from "../longPress";

describe("exceedsTouchSlop (#2232)", () => {
  const start = { x: 100, y: 100 };

  it("treats a stationary touch as within slop", () => {
    expect(exceedsTouchSlop(start, { x: 100, y: 100 })).toBe(false);
  });

  it("tolerates normal finger jitter under the slop so the long-press survives", () => {
    // A real hold wobbles a few px; the old cancel-on-any-move killed the menu.
    expect(exceedsTouchSlop(start, { x: 103, y: 98 })).toBe(false);
    expect(exceedsTouchSlop(start, { x: 95, y: 104 })).toBe(false);
  });

  it("does not cancel exactly at the slop boundary", () => {
    expect(exceedsTouchSlop(start, { x: 100 + LONG_PRESS_SLOP_PX, y: 100 })).toBe(false);
  });

  it("cancels once a deliberate drag passes the slop", () => {
    expect(exceedsTouchSlop(start, { x: 100 + LONG_PRESS_SLOP_PX + 1, y: 100 })).toBe(true);
    expect(exceedsTouchSlop(start, { x: 140, y: 100 })).toBe(true);
    expect(exceedsTouchSlop(start, { x: 100, y: 60 })).toBe(true);
  });

  it("uses 8px to match the dnd-kit TouchSensor reorder tolerance", () => {
    expect(LONG_PRESS_SLOP_PX).toBe(8);
  });
});

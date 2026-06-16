// Long-press gesture tolerance for sidebar session rows.
//
// A finger never holds perfectly still during a long-press. Cancelling the
// pending long-press on any movement (the old behavior) meant the rename/delete
// menu only opened when you pressed unnaturally precisely; otherwise Android's
// slop-tolerant native link menu won the gesture instead (#2232). We only treat
// the press as cancelled once movement exceeds this slop, the same 8px the
// dnd-kit TouchSensor uses for its reorder activation tolerance, so a normal
// jittery hold still arms the menu while a deliberate drag (scroll/reorder)
// still cancels it.
export const LONG_PRESS_SLOP_PX = 8;

export function exceedsTouchSlop(
  start: { x: number; y: number },
  point: { x: number; y: number },
  slop = LONG_PRESS_SLOP_PX,
): boolean {
  return Math.hypot(point.x - start.x, point.y - start.y) > slop;
}

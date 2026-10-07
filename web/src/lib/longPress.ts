// Movement allowed before a long-press cancels, matching dnd-kit's TouchSensor, so a jittery hold still opens the menu.
export const LONG_PRESS_SLOP_PX = 8;

export function exceedsTouchSlop(
  start: { x: number; y: number },
  point: { x: number; y: number },
  slop = LONG_PRESS_SLOP_PX,
): boolean {
  return Math.hypot(point.x - start.x, point.y - start.y) > slop;
}

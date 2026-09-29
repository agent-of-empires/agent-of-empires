export const JOYSTICK_DEAD_ZONE_PX = 12;
export const JOYSTICK_FIRST_REPEAT_MS = 400;
export const JOYSTICK_SLOW_REPEAT_MS = 150;
export const JOYSTICK_FAST_REPEAT_MS = 40;
/** Drag distance at which the repeat reaches its fastest rate. */
export const JOYSTICK_FULL_SPEED_PX = 60;

export type Direction = "up" | "down" | "left" | "right";

export const JOYSTICK_ARROWS: Record<Direction, string> = {
  up: "\x1b[A",
  down: "\x1b[B",
  right: "\x1b[C",
  left: "\x1b[D",
};

/** The dominant axis past the dead zone, or null inside it. */
export function joystickDirection(dx: number, dy: number): Direction | null {
  if (Math.max(Math.abs(dx), Math.abs(dy)) < JOYSTICK_DEAD_ZONE_PX) return null;
  if (Math.abs(dx) > Math.abs(dy)) return dx > 0 ? "right" : "left";
  return dy > 0 ? "down" : "up";
}

/** Repeat interval shrinks linearly from slow at the dead zone to fast at full speed distance. */
export function joystickRepeatMs(distance: number): number {
  const span = JOYSTICK_FULL_SPEED_PX - JOYSTICK_DEAD_ZONE_PX;
  const t = Math.min(1, Math.max(0, (distance - JOYSTICK_DEAD_ZONE_PX) / span));
  return Math.round(JOYSTICK_SLOW_REPEAT_MS - t * (JOYSTICK_SLOW_REPEAT_MS - JOYSTICK_FAST_REPEAT_MS));
}

/** Width reserved beside the terminal for the forward-mode scrollbar. */
export const FORWARD_SCROLLBAR_WIDTH_PX = 18;
/** Fastest thumb scroll (notches per second); each notch waits on a remote redraw. */
const MAX_RATE = 24;
/** Fraction of thumb travel from centre that scrolls nothing. */
const DEAD_ZONE = 0.12;
const RAMP_EXPONENT = 1.5;

/** Signed notches per second for a thumb dragged `offsetPx` from centre; negative scrolls up. */
export function forwardScrollRate(offsetPx: number, travelPx: number): number {
  if (!(travelPx > 0) || !Number.isFinite(offsetPx)) return 0;
  const n = Math.max(-1, Math.min(1, offsetPx / travelPx));
  const past = (Math.abs(n) - DEAD_ZONE) / (1 - DEAD_ZONE);
  return past <= 0 ? 0 : Math.sign(n) * MAX_RATE * past ** RAMP_EXPONENT;
}

/** One page of wheel notches, keeping a line of context. */
export function forwardPageNotches(rows: number): number {
  return Math.max(1, Math.floor(rows) - 1);
}

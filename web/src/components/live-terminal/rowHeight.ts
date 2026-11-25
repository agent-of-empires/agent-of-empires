export const DEFAULT_LINE_RATIO = 1.2;

/** Row height in whole CSS pixels: a fractional height starts rows between device pixels, unevenly spacing and
 *  blurring stacked rows, and leaves scrollHeight fractional for the live-edge checks. */
export function rowHeight(fontSize: number, lineRatio = DEFAULT_LINE_RATIO): number {
  return Math.max(1, Math.round(fontSize * lineRatio));
}

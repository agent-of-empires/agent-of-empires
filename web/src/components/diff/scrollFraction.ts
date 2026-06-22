import { changedLines } from "./find/changedLines";

const clamp01 = (n: number) => Math.min(1, Math.max(0, n));

/** Approximate scroll fraction (0..1) for a new-side source line. The Pierre
 *  Virtualizer has no scroll-to-line API, so we map the cited line to the
 *  rendered diff rows: find the nearest changed new-side line and return its
 *  rank among all rendered changed rows (deletions and additions, in render
 *  order). This tracks where the diff content actually sits, unlike a raw
 *  line/total-lines fraction which is wrong when unchanged regions are
 *  collapsed. Falls back to a clamped file-line fraction when the diff has no
 *  changed new-side rows (e.g. a deletion-only patch). See #1809. */
export function targetScrollFraction(
  meta: Parameters<typeof changedLines>[0],
  targetLine: number,
  newLineCount: number,
): number {
  const lines = changedLines(meta);
  let bestIdx = -1;
  let bestDist = Infinity;
  for (let i = 0; i < lines.length; i++) {
    if (lines[i]!.side !== "new") continue;
    const dist = Math.abs(lines[i]!.lineNumber - targetLine);
    if (dist < bestDist) {
      bestDist = dist;
      bestIdx = i;
    }
  }
  if (bestIdx < 0) return clamp01((targetLine - 1) / Math.max(1, newLineCount));
  if (lines.length <= 1) return 0;
  return bestIdx / (lines.length - 1);
}

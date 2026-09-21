import { changedLines } from "./find/changedLines";

const clamp01 = (n: number) => Math.min(1, Math.max(0, n));

/** Scroll fraction for a new-side line: the rank of the nearest changed new-side row among
 *  rendered changed rows, falling back to a clamped line fraction. */
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

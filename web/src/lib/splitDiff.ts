import type { RichDiffHunk, RichDiffLine } from "./types";

/** One row of a side-by-side diff. `left` is the old/before side, `right`
 *  the new/after side; either is null when that side has no line for the
 *  row (a placeholder cell). `leftIndex`/`rightIndex` are the line's index
 *  within the source hunk's `lines`, used to look up its syntax tokens. */
export interface SplitRow {
  left: RichDiffLine | null;
  right: RichDiffLine | null;
  leftIndex: number | null;
  rightIndex: number | null;
}

interface Indexed {
  line: RichDiffLine;
  index: number;
}

/** Pair a hunk's ordered unified lines into side-by-side rows.
 *
 * A maximal run of non-equal lines is one change block: its deletions and
 * additions are zipped index-by-index, so row k holds the k-th deletion on
 * the left and the k-th addition on the right. Surplus lines on the longer
 * side get a placeholder opposite them. Equal lines appear on both sides. */
export function buildSplitRows(hunk: RichDiffHunk): SplitRow[] {
  const rows: SplitRow[] = [];
  let dels: Indexed[] = [];
  let adds: Indexed[] = [];

  const flush = () => {
    const n = Math.max(dels.length, adds.length);
    for (let k = 0; k < n; k++) {
      const d = dels[k];
      const a = adds[k];
      rows.push({
        left: d?.line ?? null,
        right: a?.line ?? null,
        leftIndex: d?.index ?? null,
        rightIndex: a?.index ?? null,
      });
    }
    dels = [];
    adds = [];
  };

  hunk.lines.forEach((line, index) => {
    if (line.type === "equal") {
      flush();
      rows.push({
        left: line,
        right: line,
        leftIndex: index,
        rightIndex: index,
      });
    } else if (line.type === "delete") {
      dels.push({ line, index });
    } else {
      adds.push({ line, index });
    }
  });
  flush();

  return rows;
}

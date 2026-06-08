import type { FileDiffMetadata } from "@pierre/diffs";
import type { SearchableLine } from "./findMatches";

/**
 * The changed (added/deleted) lines of an already-parsed diff, in rendered
 * (top-to-bottom, deletions-before-additions per change block) order.
 *
 * This is the searchable set for in-diff find: for the MVP we match only lines
 * that are actually part of the change, not unchanged context or the rest of
 * the file. Expanded-context lines (which the user can reveal) are out of
 * scope for now.
 */
export function changedLines(meta: FileDiffMetadata): SearchableLine[] {
  const out: SearchableLine[] = [];
  for (const hunk of meta.hunks) {
    for (const seg of hunk.hunkContent) {
      if (seg.type !== "change") continue;
      for (let k = 0; k < seg.deletions; k++) {
        const idx = seg.deletionLineIndex + k;
        out.push({
          side: "old",
          lineNumber: idx + 1,
          text: stripNewline(meta.deletionLines[idx] ?? ""),
        });
      }
      for (let k = 0; k < seg.additions; k++) {
        const idx = seg.additionLineIndex + k;
        out.push({
          side: "new",
          lineNumber: idx + 1,
          text: stripNewline(meta.additionLines[idx] ?? ""),
        });
      }
    }
  }
  return out;
}

// Pierre keeps the trailing newline on each stored line; strip it so find
// matches and column offsets are line-text accurate.
function stripNewline(s: string): string {
  return s.replace(/\r?\n$/, "");
}

import type { FileDiffMetadata } from "@pierre/diffs";
import type { SearchableLine } from "./findMatches";

/** Added and deleted lines in render order: the searchable set for in-diff find. */
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

// Stored lines keep their trailing newline.
function stripNewline(s: string): string {
  return s.replace(/\r?\n$/, "");
}

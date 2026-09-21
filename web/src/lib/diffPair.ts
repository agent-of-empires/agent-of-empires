// Line-diff an `(old_string, new_string)` pair into a `RichDiffHunk` plus counts, using the `@pierre/diffs` engine.

import { parseDiffFromFile } from "@pierre/diffs";
import type { RichDiffHunk, RichDiffLine } from "./types";

export interface DiffPairResult {
  hunk: RichDiffHunk;
  adds: number;
  dels: number;
}

/** Without a shared trailing newline, appending a line diffs as a changed last line. */
function withTrailingNewline(s: string): string {
  if (s === "") return s;
  return s.endsWith("\n") ? s : s + "\n";
}

/** `parseDiffFromFile` keeps newlines on every line but the last; strip them (and CRLF's `\r`). */
function stripNewline(s: string): string {
  return s.replace(/\r?\n$/, "");
}

/** Line numbers start at 1 on each side. */
export function diffPair(oldText: string, newText: string): DiffPairResult {
  if (oldText === "" && newText === "") {
    return {
      hunk: {
        old_start: 0,
        old_lines: 0,
        new_start: 0,
        new_lines: 0,
        lines: [],
      },
      adds: 0,
      dels: 0,
    };
  }

  const oldNormalized = withTrailingNewline(oldText);
  const newNormalized = withTrailingNewline(newText);

  const lines: RichDiffLine[] = [];
  let oldNum = 1;
  let newNum = 1;
  let adds = 0;
  let dels = 0;

  if (oldNormalized === newNormalized) {
    // Identical content has no hunks; show every line as equal.
    for (const content of stripNewline(oldNormalized).split(/\r?\n/)) {
      lines.push({
        type: "equal",
        old_line_num: oldNum++,
        new_line_num: newNum++,
        content,
      });
    }
  } else {
    // A huge context keeps every unchanged line, so the snippet renders in full.
    const meta = parseDiffFromFile(
      { name: "f", contents: oldNormalized },
      { name: "f", contents: newNormalized },
      { context: Number.MAX_SAFE_INTEGER },
    );

    for (const hunk of meta.hunks) {
      for (const segment of hunk.hunkContent) {
        if (segment.type === "context") {
          const idx = segment.additionLineIndex;
          for (const raw of meta.additionLines.slice(idx, idx + segment.lines)) {
            lines.push({
              type: "equal",
              old_line_num: oldNum++,
              new_line_num: newNum++,
              content: stripNewline(raw),
            });
          }
        } else {
          const delIdx = segment.deletionLineIndex;
          for (const raw of meta.deletionLines.slice(delIdx, delIdx + segment.deletions)) {
            lines.push({
              type: "delete",
              old_line_num: oldNum++,
              new_line_num: null,
              content: stripNewline(raw),
            });
            dels += 1;
          }
          const addIdx = segment.additionLineIndex;
          for (const raw of meta.additionLines.slice(addIdx, addIdx + segment.additions)) {
            lines.push({
              type: "add",
              old_line_num: null,
              new_line_num: newNum++,
              content: stripNewline(raw),
            });
            adds += 1;
          }
        }
      }
    }
  }

  const oldLines = oldNum - 1;
  const newLines = newNum - 1;

  return {
    hunk: {
      old_start: oldLines > 0 ? 1 : 0,
      old_lines: oldLines,
      new_start: newLines > 0 ? 1 : 0,
      new_lines: newLines,
      lines,
    },
    adds,
    dels,
  };
}

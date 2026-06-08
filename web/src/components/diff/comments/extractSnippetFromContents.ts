import type { DiffSide } from "./types";

/**
 * Pull the text for a 1-based inclusive line range from the raw old/new file
 * contents.
 *
 * The contents-based renderer (`@pierre/diffs`) is fed full file text instead
 * of pre-computed hunks, so a comment's range maps directly onto line numbers
 * of one side's content. Returns `null` when the range falls outside that
 * side's line count (the comment is then treated as stale).
 *
 * Side semantics mirror the hunk version: `old` reads the deletion/base text,
 * `new` reads the addition/working text.
 */
export function extractSnippetFromContents(
  oldContent: string,
  newContent: string,
  side: DiffSide,
  startLine: number,
  endLine: number,
): string | null {
  const lo = Math.min(startLine, endLine);
  const hi = Math.max(startLine, endLine);
  if (lo < 1) return null;

  const lines = splitLines(side === "new" ? newContent : oldContent);
  if (hi > lines.length) return null;

  return lines.slice(lo - 1, hi).join("\n");
}

function splitLines(content: string): string[] {
  if (content.length === 0) return [];
  const lines = content.split("\n");
  // A trailing newline yields a spurious final "" entry; drop it so line
  // counts match the file's real line count.
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  return lines;
}

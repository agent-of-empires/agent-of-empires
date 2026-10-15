import type { DiffSide } from "./types";

/** Text of a 1-based inclusive range on one side, or null when it falls outside that side. */
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
  // A trailing newline is not an extra line.
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  return lines;
}

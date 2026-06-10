import { parseAnsi, type AnsiSegment } from "./ansi";

// Frame helpers for the mobile live terminal: turn one `capture-pane -e`
// snapshot into per-line styled segments the component can render as DOM
// rows. SGR state legitimately spans lines (tmux emits a reset only when
// the style changes), so the split happens AFTER parsing, carrying each
// segment's style across the newline.

export function ansiToLines(content: string): AnsiSegment[][] {
  const segs = parseAnsi(content);
  const lines: AnsiSegment[][] = [[]];
  for (const seg of segs) {
    const parts = seg.text.split("\n");
    parts.forEach((part, i) => {
      if (i > 0) lines.push([]);
      if (part.length > 0) {
        lines[lines.length - 1]!.push({ text: part, style: seg.style });
      }
    });
  }
  // capture-pane terminates every line, including the last, with `\n`;
  // drop the phantom empty line that trailing terminator creates so the
  // last rendered row is the pane's real bottom row.
  if (lines.length > 1 && lines[lines.length - 1]!.length === 0) {
    lines.pop();
  }
  return lines;
}

/** Plain text of one rendered line (for tests / cursor math). */
export function lineText(line: AnsiSegment[]): string {
  return line.map((s) => s.text).join("");
}

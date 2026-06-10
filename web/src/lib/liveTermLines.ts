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

/** Hard-wrap one styled line at `cols` columns, preserving segment
 *  styles across the breaks. Lines at or under the limit return a
 *  single visual row (the normal case: the pane is sized to the
 *  viewer's grid, so this is the identity). Wider lines appear when
 *  another writer resized the tmux window out from under the viewer;
 *  wrapping keeps them readable until the server re-asserts the grid. */
export function wrapLine(line: AnsiSegment[], cols: number): AnsiSegment[][] {
  if (!Number.isFinite(cols) || cols <= 0) return [line];
  const total = line.reduce((n, s) => n + s.text.length, 0);
  if (total <= cols) return [line];
  const rows: AnsiSegment[][] = [];
  let current: AnsiSegment[] = [];
  let used = 0;
  for (const seg of line) {
    let text = seg.text;
    while (text.length > 0) {
      const space = cols - used;
      if (space === 0) {
        rows.push(current);
        current = [];
        used = 0;
        continue;
      }
      const take = text.slice(0, space);
      current.push({ text: take, style: seg.style });
      used += take.length;
      text = text.slice(space);
    }
  }
  if (current.length > 0 || rows.length === 0) rows.push(current);
  return rows;
}

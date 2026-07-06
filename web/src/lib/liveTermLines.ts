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

// Terminal cell widths, wcwidth-style: combining marks and zero-width
// joiners take no cell; East Asian Wide/Fullwidth and emoji take two.
// tmux wraps by cells, so wrapping (and the cursor math built on it)
// must count the same way, not in UTF-16 code units.
const ZERO_WIDTH = /[\u200B-\u200D\uFEFF]|\p{M}/u;
const WIDE =
  /[\u1100-\u115F\u2E80-\u303E\u3041-\u33FF\u3400-\u4DBF\u4E00-\u9FFF\uA000-\uA4CF\uAC00-\uD7A3\uF900-\uFAFF\uFE30-\uFE4F\uFF00-\uFF60\uFFE0-\uFFE6\u{1F300}-\u{1FAFF}]|\p{Emoji_Presentation}/u;
const ASCII_PRINTABLE_ONLY = /^[\x20-\x7E]*$/;

export function cellWidth(codePoint: string): number {
  if (ZERO_WIDTH.test(codePoint)) return 0;
  return WIDE.test(codePoint) ? 2 : 1;
}

export function textWidth(text: string): number {
  if (ASCII_PRINTABLE_ONLY.test(text)) return text.length;
  let width = 0;
  for (const ch of text) width += cellWidth(ch);
  return width;
}

/** Find the code point in `text` whose terminal cell range contains `col`
 *  (cell units from the start of `text`), or null if `col` falls at or
 *  past the end. Iterates code points (an emoji's surrogate pair never
 *  splits) and counts cells the same way `textWidth`/`wrapLine` do, so a
 *  cursor column from tmux (already cell-based) lands on the right glyph
 *  even when wide CJK or zero-width characters precede it. */
export function findCursorCharIndex(text: string, col: number): number | null {
  if (ASCII_PRINTABLE_ONLY.test(text)) {
    return col >= 0 && col < text.length ? col : null;
  }
  let c = 0;
  let i = 0;
  for (const ch of text) {
    const w = cellWidth(ch);
    if (col >= c && col < c + w) return i;
    c += w;
    i++;
  }
  return null;
}

/** Hard-wrap one styled line at `cols` terminal cells, preserving
 *  segment styles across the breaks. Lines at or under the limit return
 *  a single visual row (the normal case: the pane is sized to the
 *  viewer's grid, so this is the identity). Wider lines appear when
 *  another writer resized the tmux window out from under the viewer;
 *  wrapping keeps them readable until the server re-asserts the grid.
 *  Iterates code points (an emoji's surrogate pair never splits) and
 *  counts cells, so CJK and emoji wrap where tmux would wrap them. */
export function wrapLine(line: AnsiSegment[], cols: number): AnsiSegment[][] {
  if (!Number.isFinite(cols) || cols <= 0) return [line];
  const total = line.reduce((n, s) => n + textWidth(s.text), 0);
  if (total <= cols) return [line];
  const rows: AnsiSegment[][] = [];
  let current: AnsiSegment[] = [];
  let used = 0;
  for (const seg of line) {
    let chunk = "";
    const flushChunk = () => {
      if (chunk.length > 0) {
        current.push({ text: chunk, style: seg.style });
        chunk = "";
      }
    };
    for (const ch of seg.text) {
      const w = cellWidth(ch);
      // A wide char that doesn't fit wraps whole (terminals leave the
      // last cell empty); zero-width marks stay with their base char.
      if (used + w > cols && used > 0) {
        flushChunk();
        rows.push(current);
        current = [];
        used = 0;
      }
      chunk += ch;
      used += w;
    }
    flushChunk();
  }
  if (current.length > 0 || rows.length === 0) rows.push(current);
  return rows;
}

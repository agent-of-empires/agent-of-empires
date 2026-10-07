import { Fragment, memo, type CSSProperties, type ReactNode } from "react";
import type { AnsiSegment, AnsiStyle } from "../../lib/ansi";
import {
  clusterSpanAt,
  findCursorCharIndex,
  isHttpUrl,
  splitCellRuns,
  splitUrls,
  textWidth,
} from "../../lib/liveTermLines";

function segStyle(style: AnsiStyle): CSSProperties | undefined {
  const css: CSSProperties = {};
  let fg = style.fg;
  let bg = style.bg;
  if (style.inverse) {
    [fg, bg] = [bg ?? "var(--term-bg, #1c1c1f)", fg ?? "var(--term-fg, #e4e4e7)"];
  }
  if (fg) css.color = fg;
  if (bg) css.backgroundColor = bg;
  if (style.bold) css.fontWeight = 700;
  if (style.dim) css.opacity = 0.6;
  if (style.italic) css.fontStyle = "italic";
  if (style.underline) css.textDecoration = "underline";
  return Object.keys(css).length ? css : undefined;
}

/** An inline box exactly `cells` wide, so a fallback font's advance error cannot shift later glyphs. */
function fixedBoxStyle(cells: number, base: CSSProperties | undefined): CSSProperties {
  return { ...base, display: "inline-block", width: `calc(var(--term-cell, 1em) * ${cells})` };
}

function runSpan(key: string | number, text: string, fixed: boolean, cells: number, base: CSSProperties | undefined) {
  return (
    <span key={key} style={fixed ? fixedBoxStyle(cells, base) : base}>
      {text}
    </span>
  );
}

// The cursor is a cell in the text flow: filled while the input is focused, a hollow outline otherwise.
const CURSOR_FOCUSED: CSSProperties = {
  backgroundColor: "var(--term-cursor, #f59e0b)",
  color: "var(--term-bg, #1c1c1f)",
};
const CURSOR_BLURRED: CSSProperties = { outline: "1px solid var(--term-cursor, #f59e0b)", outlineOffset: "-1px" };

function cursorSpan(key: string | number, text: string, base: CSSProperties | undefined, focused: boolean) {
  return (
    <span
      key={key}
      data-live-cursor
      className={focused ? "animate-term-cursor-blink" : undefined}
      style={{ ...fixedBoxStyle(textWidth(text), base), ...(focused ? CURSOR_FOCUSED : CURSOR_BLURRED) }}
    >
      {text}
    </span>
  );
}

function LinkedRow({ segs }: { segs: AnsiSegment[] }) {
  if (segs.length === 0) return <div> </div>; // keep empty rows at full height
  return (
    <div>
      {segs.map((seg, i) =>
        // An OSC 8 link's text need not be its URL, so it anchors whole instead of being re-scanned.
        (seg.url && isHttpUrl(seg.url) ? [{ text: seg.text, url: seg.url }] : splitUrls(seg.text)).map((part, j) => {
          const runs = splitCellRuns(part.text).map((run, k) =>
            runSpan(`${i}-${j}-${k}`, run.text, run.fixed, run.cells, segStyle(seg.style)),
          );
          if (!part.url) return <Fragment key={`${i}-${j}`}>{runs}</Fragment>;
          return (
            <a
              key={`${i}-${j}`}
              href={part.url}
              target="_blank"
              rel="noopener noreferrer"
              className="underline cursor-pointer"
            >
              {runs}
            </a>
          );
        }),
      )}
    </div>
  );
}

/** One visual row. The cursor row walks terminal cells (wide glyphs take two) and is not linkified. */
export const Row = memo(function Row({
  segs,
  cursorCol,
  focused = false,
}: {
  segs: AnsiSegment[];
  cursorCol: number | null;
  focused?: boolean;
}) {
  if (cursorCol == null) return <LinkedRow segs={segs} />;
  const out: ReactNode[] = [];
  let col = 0;
  let placed = false;
  let key = 0;
  for (const seg of segs) {
    for (const run of splitCellRuns(seg.text)) {
      const end = col + run.cells;
      const base = segStyle(seg.style);
      const idx =
        !placed && cursorCol >= col && cursorCol < end ? findCursorCharIndex(run.text, cursorCol - col) : null;
      if (idx == null) {
        out.push(runSpan(key++, run.text, run.fixed, run.cells, base));
      } else {
        placed = true;
        const chars = [...run.text];
        // The cursor takes the whole grapheme cluster, so no combining tail is stranded in a sibling.
        const [clusterStart, clusterEnd] = clusterSpanAt(run.text, idx);
        const pre = chars.slice(0, clusterStart).join("");
        const post = chars.slice(clusterEnd).join("");
        if (clusterStart > 0) out.push(runSpan(key++, pre, run.fixed, textWidth(pre), base));
        out.push(cursorSpan(key++, chars.slice(clusterStart, clusterEnd).join(""), base, focused));
        if (clusterEnd < chars.length) out.push(runSpan(key++, post, run.fixed, textWidth(post), base));
      }
      col = end;
    }
  }
  if (!placed) {
    // Past the row's text: pad with a boxed run of spaces, then a boxed blank cursor cell.
    if (cursorCol > col) out.push(runSpan("pad", " ".repeat(cursorCol - col), true, cursorCol - col, undefined));
    out.push(cursorSpan("cursor", " ", undefined, focused));
  }
  return <div>{out}</div>;
});

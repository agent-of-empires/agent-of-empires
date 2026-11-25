import type { CSSProperties } from "react";
import type { AnsiStyle } from "../../lib/ansi";

const TERM_FG = "var(--term-fg, #e4e4e7)";
const TERM_BG = "var(--term-bg, #1c1c1f)";

export function segStyle(style: AnsiStyle): CSSProperties | undefined {
  const css: CSSProperties = {};
  let fg = style.fg;
  let bg = style.bg;
  if (style.inverse) {
    [fg, bg] = [bg ?? TERM_BG, fg ?? TERM_FG];
  }
  // Dim blends the glyph colour toward its own background: opacity would also fade the cell background.
  if (style.dim) css.color = `color-mix(in srgb, ${fg ?? TERM_FG} 60%, ${bg ?? TERM_BG})`;
  else if (fg) css.color = fg;
  if (bg) css.backgroundColor = bg;
  if (style.bold) css.fontWeight = 700;
  if (style.italic) css.fontStyle = "italic";
  if (style.underline) css.textDecoration = "underline";
  return Object.keys(css).length ? css : undefined;
}

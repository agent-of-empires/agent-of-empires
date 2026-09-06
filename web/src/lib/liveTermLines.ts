import { parseAnsi, parseAnsiFrom, type AnsiSegment, type AnsiStyle } from "./ansi";

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

interface CachedLine {
  segs: AnsiSegment[];
  /** SGR state left in effect after this line, threaded into the next. */
  exit: AnsiStyle;
}

/** Key for the SGR state a line is entered with. Two identical raw lines
 *  parsed under different carried styles are different render results, so
 *  the entry state is part of the cache key. */
function styleKey(s: AnsiStyle): string {
  return `${s.fg ?? ""}|${s.bg ?? ""}|${+!!s.bold}${+!!s.dim}${+!!s.italic}${+!!s.underline}${+!!s.inverse}`;
}

/**
 * Frame-to-frame parse cache for [`ansiToLines`]-equivalent output.
 *
 * A streamed capture frame is byte-identical to the previous one on almost
 * every line (only the tail moves), yet re-parsing the whole window per
 * frame made every line's segment arrays fresh objects, which both burned
 * main-thread time on multi-thousand-line reading windows and defeated the
 * row memoization downstream (every mounted row re-rendered per frame, the
 * scroll-jank driver on phones). `lines()` parses per line, keyed on
 * (entry SGR state, raw line), and returns the SAME segment arrays for
 * unchanged lines, so identity-based memo and WeakMap caches hold.
 *
 * Two-generation eviction: entries used by the current frame move to the
 * live generation; the rest are dropped when the next frame arrives.
 * Memory is bounded to two frames' unique lines, and re-running on the
 * same content (React StrictMode double-invoke) converges to identical
 * output and identities.
 */
export class LineParseCache {
  private live = new Map<string, CachedLine>();
  private prev = new Map<string, CachedLine>();

  lines(content: string | readonly string[]): AnsiSegment[][] {
    this.prev = this.live;
    this.live = new Map();
    const raw = typeof content === "string" ? content.split("\n") : content;
    const lines: AnsiSegment[][] = [];
    let entry: AnsiStyle = {};
    for (const r of raw) {
      // NUL separator: it appears in neither a style key (CSS color
      // strings) nor capture-pane text, so the key cannot be ambiguous.
      const key = styleKey(entry) + "\u0000" + r;
      let hit = this.live.get(key) ?? this.prev.get(key);
      if (!hit) {
        const parsed = parseAnsiFrom(r, entry);
        hit = { segs: parsed.segs, exit: parsed.exit };
      }
      this.live.set(key, hit);
      lines.push(hit.segs);
      entry = hit.exit;
    }
    // Mirror ansiToLines: capture-pane terminates every line, including
    // the last, with `\n`; drop the phantom empty line that creates. A row
    // array from the hook has already had it removed.
    if (typeof content === "string" && lines.length > 1 && lines[lines.length - 1]!.length === 0) {
      lines.pop();
    }
    return lines;
  }
}

/** Plain text of one rendered line (for tests / cursor math). */
export function lineText(line: AnsiSegment[]): string {
  return line.map((s) => s.text).join("");
}

// Match http(s) URLs so agent output in the terminal view can be linkified.
// ponytail: plain per-line regex, no OSC 8 / reflow tracking (there is no
// xterm here). A URL split across wrapped visual rows linkifies only its
// first part; upgrade to reflow-aware matching only if that proves painful.
// The match may run into glued non-ASCII glyphs; Row anchors whole parts,
// so the href follows whatever this regex claims.
const URL_RE = /https?:\/\/\S+/g;
// Trailing punctuation that is usually sentence/wrapping syntax, not the URL
// (e.g. `see https://x.com/a).`). Stripped from the match; re-emitted as text.
const URL_TRAILING = /[.,;:!?)\]}'">]+$/;

export interface UrlPart {
  text: string;
  /** The href when this part is a link, else null. */
  url: string | null;
}

/** Split one line of plain text into link and non-link parts. Returns a
 *  single non-link part when there are no URLs. */
export function splitUrls(text: string): UrlPart[] {
  const parts: UrlPart[] = [];
  let last = 0;
  for (const m of text.matchAll(URL_RE)) {
    const start = m.index;
    const raw = m[0];
    const trimmed = raw.replace(URL_TRAILING, "");
    // Keep the trimmed form only if a host character survives; otherwise the
    // match was scheme + punctuation and the original stands.
    const url = /^https?:\/\/\S/.test(trimmed) ? trimmed : raw;
    if (start > last) parts.push({ text: text.slice(last, start), url: null });
    parts.push({ text: url, url });
    last = start + url.length;
  }
  if (parts.length === 0) return [{ text, url: null }];
  if (last < text.length) parts.push({ text: text.slice(last), url: null });
  return parts;
}

// Terminal cell widths, tmux-aligned and counted PER GRAPHEME CLUSTER,
// not per code point (measured against tmux 3.6a cursor_x deltas):
// combining marks, zero-width joiners and variation selectors take no
// column of their own, and neither does a skin-tone swatch that has a
// modifier base in front of it; a VS16-forced emoji, a flag pair and a
// ZWJ chain each take exactly two columns; a lone regional indicator
// takes one; East Asian Wide/Fullwidth and emoji take two. An orphan
// mark with no base to attach to keeps whatever width it has on its own.
const ZERO_WIDTH = /[\u200B-\u200D\uFEFF]|\p{M}/u;
// Emoji composition tails: they modify the preceding pictographic glyph
// and occupy no column of their own (text/color variation selectors).
const EMOJI_TAIL = /^[\uFE0E\uFE0F]$/u;
// Skin-tone swatches fold into the preceding glyph only when that glyph
// takes a modifier; after anything else the swatch keeps its own two
// columns (measured: thumbs-up + tone = 2 cells, grinning face + tone =
// 4, since U+1F600 takes no modifier). tmux 3.6a decides this from a
// hand-maintained list of ~70 base code points in utf8_should_combine()
// rather than from Unicode's Emoji_Modifier_Base property, so a base in
// the property but missing from that list (U+270B, U+1F91D, U+1F3C3)
// still measures 2 here against tmux's 4. The property is the closest
// stable approximation and errs only on that gap.
const SKIN_TONE = /^[\u{1F3FB}-\u{1F3FF}]$/u;
const MODIFIER_BASE = /\p{Emoji_Modifier_Base}/u;
// Regional indicator pairs compose flag glyphs.
const REGIONAL_INDICATOR = /[\u{1F1E6}-\u{1F1FF}]/u;
const WIDE =
  /[\u1100-\u115F\u2E80-\u303E\u3041-\u33FF\u3400-\u4DBF\u4E00-\u9FFF\uA000-\uA4CF\uAC00-\uD7A3\uF900-\uFAFF\uFE30-\uFE4F\uFF00-\uFF60\uFFE0-\uFFE6\u{1F300}-\u{1FAFF}]|\p{Emoji_Presentation}/u;
const ASCII_PRINTABLE_ONLY = /^[\x20-\x7E]*$/;
const ZWJ = "\u200D";

export function cellWidth(codePoint: string): number {
  if (ZERO_WIDTH.test(codePoint) || EMOJI_TAIL.test(codePoint)) return 0;
  return WIDE.test(codePoint) ? 2 : 1;
}

/** True when `next` composes onto a cluster based on `base` instead of
 *  opening a column of its own. Shared by splitGraphemes (which decides
 *  cluster widths) and clusterSpanAt (which decides how much text the
 *  boxed cursor cell takes), so the two cannot drift apart. */
function composesOnto(base: string, next: string): boolean {
  if (SKIN_TONE.test(next)) return MODIFIER_BASE.test(base);
  return ZERO_WIDTH.test(next) || EMOJI_TAIL.test(next);
}

/** Split text into extended grapheme clusters: a base glyph plus its
 *  zero-width marks and emoji tails; consecutive regional indicators
 *  pair up on parity into flags; a ZWJ joins the next non-ASCII glyph
 *  into the same cluster. Mirrors clusterSpanAt's absorption rules. */
function splitGraphemes(text: string): string[] {
  const clusters: string[] = [];
  const chars = [...text];
  let i = 0;
  while (i < chars.length) {
    const base = chars[i]!;
    let cluster = base;
    if (chars[i + 1] === "\uFE0F" && chars[i + 2] === "\u20E3") {
      // Keycap sequence (base + VS16 + enclosing keycap): one two-cell
      // cluster even when the base is printable ASCII like #, * or a
      // digit.
      cluster += chars[++i]!;
      cluster += chars[++i]!;
    } else if (REGIONAL_INDICATOR.test(cluster)) {
      // Pair on parity within the maximal RI run.
      let runStart = i;
      while (runStart > 0 && REGIONAL_INDICATOR.test(chars[runStart - 1]!)) runStart--;
      if ((i - runStart) % 2 === 0 && REGIONAL_INDICATOR.test(chars[i + 1] ?? "")) {
        cluster += chars[++i]!;
      }
    } else if (!ASCII_PRINTABLE_ONLY.test(cluster)) {
      // Non-ASCII base: absorb trailing marks, emoji tails and whatever
      // non-ASCII glyph a ZWJ joins into the same cluster.
      for (;;) {
        const next = chars[i + 1];
        if (next === undefined) break;
        if (next === ZWJ) {
          cluster += next;
          i++;
          const joined = chars[i + 1];
          if (joined === undefined || ASCII_PRINTABLE_ONLY.test(joined)) break;
          cluster += joined;
          i++;
          continue;
        }
        if (composesOnto(base, next)) {
          cluster += next;
          i++;
          continue;
        }
        break;
      }
    }
    clusters.push(cluster);
    i++;
  }
  return clusters;
}

function graphemeWidth(cluster: string): number {
  const cps = [...cluster];
  const riCount = cps.filter((c) => REGIONAL_INDICATOR.test(c)).length;
  // The keycap and ZWJ rules need a base to apply to: an orphan U+20E3 or
  // U+200D with nothing in front of it is an ordinary zero-width mark, so
  // both guards require more than one code point in the cluster.
  if (cps.length > 1 && cps.some((c) => c === "\u20E3")) return 2;
  if (riCount >= 2) return 2;
  if (riCount === 1 && cps.length === 1) return 1;
  if (cps.length > 1 && cps.some((c) => c === ZWJ)) return 2;
  if (cps.length > 1 && cps[cps.length - 1] === "\uFE0F") return 2;
  return cellWidth(cps[0]!);
}

/** Terminal cells occupied by a whole line: the sum of its grapheme
 *  clusters' widths, matching how tmux counts columns. */
export function textWidth(text: string): number {
  if (ASCII_PRINTABLE_ONLY.test(text)) return text.length;
  return splitGraphemes(text).reduce((n, c) => n + graphemeWidth(c), 0);
}

/** Find the code point that starts the grapheme cluster whose terminal
 *  cell range contains `col`, or null if `col` falls at or past the end.
 *  Counts cells per cluster (a flag, ZWJ chain or VS16-forced emoji is
 *  one stop of width 2), so a cursor column from tmux lands on the right
 *  glyph even when wide CJK or zero-width characters precede it. */
export function findCursorCharIndex(text: string, col: number): number | null {
  let c = 0;
  let pos = 0;
  for (const cluster of splitGraphemes(text)) {
    const w = graphemeWidth(cluster);
    if (col >= c && col < c + w) return pos;
    c += w;
    pos += [...cluster].length;
  }
  return null;
}

/** One renderable piece of a row. Consecutive printable ASCII flows
 *  naturally (`fixed: false`, the configured monospace font is trusted for
 *  the only range where every plausible fallback agrees); each contiguous
 *  stretch of everything else (CJK, braille, powerline PUA, emoji, box
 *  drawing, RTL or joining scripts) becomes ONE explicitly sized box of
 *  `cells x cellWidth`. A glyph missing from the configured font falls
 *  back to a font whose advance is not 1 cell; the box pins every
 *  flow/fixed boundary to its exact column (#3342), so a row of N cells
 *  lays out N x cellWidth regardless of which font supplied each glyph.
 *  cellWidth is measured at regular weight; on systems where the
 *  configured font has no true bold face, synthesized bold can advance
 *  slightly wider and bold runs may drift inside their boxes.
 *  Whole stretches stay in a single text node because atomic inline
 *  boundaries would otherwise break Unicode bidi reordering, complex
 *  script shaping and emoji composition inside the stretch. */
export interface CellRun {
  text: string;
  /** Terminal cells this run occupies (zero-width marks add none). */
  cells: number;
  /** Render inside an explicit `cells x cellWidth` box. */
  fixed: boolean;
}

/** Split one line's text into runs of like rendering risk. Zero-width
 *  characters and emoji tails glue onto the preceding run (marks must
 *  shape with their base or browsers draw dotted-circle placeholders);
 *  leading marks with no base open their own zero-cell flow run. A fixed
 *  stretch swallows every contiguous non-ASCII code point, so composed
 *  glyphs (flag pairs, skin tones, ZWJ chains) and script shaping stay
 *  whole inside one text node. */
export function splitCellRuns(text: string): CellRun[] {
  const runs: CellRun[] = [];
  let flow = "";
  let fixedStretch = "";
  const flushFlow = () => {
    if (flow) {
      runs.push({ text: flow, cells: textWidth(flow), fixed: false });
      flow = "";
    }
  };
  const flushFixed = () => {
    if (fixedStretch) {
      runs.push({ text: fixedStretch, cells: textWidth(fixedStretch), fixed: true });
      fixedStretch = "";
    }
  };
  const chars = [...text];
  let i = 0;
  while (i < chars.length) {
    const ch = chars[i]!;
    if (ASCII_PRINTABLE_ONLY.test(ch)) {
      flushFixed();
      flow += ch;
    } else if (ZERO_WIDTH.test(ch)) {
      // Glue backward: onto the pending fixed stretch, else onto the flow
      // run (a line opening with a mark opens a zero-width flow run).
      if (fixedStretch) fixedStretch += ch;
      else flow += ch;
    } else if (EMOJI_TAIL.test(ch)) {
      // Composition tail of the previous glyph: no new box, no new cells
      // beyond what textWidth counts.
      if (fixedStretch) fixedStretch += ch;
      else flow += ch;
    } else {
      flushFlow();
      // Contiguous non-ASCII coalesces: every following code point that is
      // not printable ASCII lands here too (marks, tails, joined glyphs),
      // keeping the whole stretch in one text node.
      fixedStretch += ch;
    }
    i++;
  }
  flushFlow();
  flushFixed();
  return runs;
}

/** Code-point range `[start, end)` of the grapheme cluster containing
 *  `charIndex`: its base plus trailing zero-width marks and emoji tails,
 *  extended over a completing regional-indicator pair and across a ZWJ
 *  join, mirroring splitCellRuns' absorption rules so the cursor cell can
 *  slice a coalesced fixed stretch without stranding a composition tail
 *  outside the highlight. */
export function clusterSpanAt(text: string, charIndex: number): [number, number] {
  const chars = [...text];
  let start = charIndex;
  let end = charIndex + 1;
  const glueAt = (k: number) => k >= 0 && k < chars.length && composesOnto(chars[start] ?? "", chars[k]!);
  while (glueAt(end)) end++;
  // Regional indicators pair on parity within their maximal run, so a
  // cursor between two adjacent flags pairs with its own flag's half
  // instead of straddling the boundary.
  let runStart = start;
  while (runStart > 0 && REGIONAL_INDICATOR.test(chars[runStart - 1] ?? "")) runStart--;
  const onRi = REGIONAL_INDICATOR.test(chars[start] ?? "");
  const oddInRun = (start - runStart) % 2 === 1;
  if (onRi && (oddInRun || REGIONAL_INDICATOR.test(chars[start + 1] ?? ""))) {
    if (oddInRun) start--;
    end = Math.max(end, start + 2);
    // The pair may itself be followed by composition tails.
    while (glueAt(end)) end++;
  }
  while (chars[end - 1] === "\u200D") {
    end++;
    while (glueAt(end)) end++;
  }
  return [Math.max(start, 0), Math.min(end, chars.length)];
}

/** Hard-wrap one styled line at `cols` terminal cells, preserving
 *  segment styles across the breaks. Lines at or under the limit return
 *  a single visual row (the normal case: the pane is sized to the
 *  viewer's grid, so this is the identity). Wider lines appear when
 *  another writer resized the tmux window out from under the viewer;
 *  wrapping keeps them readable until the server re-asserts the grid.
 *  Iterates grapheme clusters and counts cells, so CJK, flags, ZWJ
 *  chains and emoji wrap where tmux would wrap them. */
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
    for (const cluster of splitGraphemes(seg.text)) {
      const w = graphemeWidth(cluster);
      // A cluster that doesn't fit wraps whole (terminals leave the last
      // cell empty); zero-width members never separate from their base.
      if (used + w > cols && used > 0) {
        flushChunk();
        rows.push(current);
        current = [];
        used = 0;
      }
      chunk += cluster;
      used += w;
    }
    flushChunk();
  }
  if (current.length > 0 || rows.length === 0) rows.push(current);
  return rows;
}

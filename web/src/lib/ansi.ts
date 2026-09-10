/* eslint-disable no-control-regex -- this file's whole job is to match ESC sequences */
// ANSI SGR parser for Bash tool output and the live terminal view.
//
// claude-agent-acp forwards `\x1b[...m` color escapes from commands
// like `git status --color=always` and `gls --color=always`. Shiki's
// bash grammar treats them as raw text, so the user sees literal
// `[01;34m` noise unless we render them ourselves. Agents also emit
// `\x1b]8;;URL\x1b\TEXT\x1b]8;;\x1b\` OSC 8 hyperlinks (e.g. `gh pr
// create` output); there is no xterm here to interpret those, so
// without this parser they show up as literal escape bytes (#3519).
//
// One pass walks every escape sequence in order, carrying the SGR style
// and the open hyperlink target as a single state. Text between sequences
// becomes a segment stamped with that state, so there are no string
// coordinates for two passes to disagree about.
//
// Carriage-return repaints are collapsed first, because that is a
// line-oriented rewrite that needs no coordinate agreement.

// Any CSI sequence: ESC [ params final-byte (any letter).
const ANY_CSI = /\[[\d;?]*[a-zA-Z]/g;

// Every escape sequence this parser understands, in one alternation so a
// single walk sees them in the order they appear:
//   1-2: CSI params + final byte. `m` is SGR; every other final byte is
//        cursor movement, line erase and the like, dropped.
//   3-4: OSC code + payload, terminated by BEL or ST (`ESC \`). Code 8
//        carries `params;URI` and opens or closes a hyperlink; every other
//        code (title sets, OSC 52 clipboard) is dropped whole, payload
//        included, because none of it is meant to render.
const TOKEN = /\[([\d;?]*)([a-zA-Z])|\]([0-9]+)(?:;([^\x07]*))?(?:\\|\x07)/g;

export interface AnsiStyle {
  fg?: string;
  bg?: string;
  bold?: boolean;
  dim?: boolean;
  italic?: boolean;
  underline?: boolean;
  inverse?: boolean;
}

export interface AnsiSegment {
  text: string;
  style: AnsiStyle;
  /** Hyperlink target when this span fell inside an OSC 8 sequence. */
  url?: string;
}

/** Everything an escape sequence can leave in effect past the end of a
 *  line: tmux emits a reset only when the style changes, and a hyperlink
 *  legitimately spans lines, so a per-line parse must thread both. */
export interface AnsiState {
  style: AnsiStyle;
  /** Target of a hyperlink still open at this point, if any. */
  url?: string;
}

// Match a real escape sequence, not just an `ESC [` or `ESC ]` prefix. A
// markdown blob that quotes the literal characters "[" — e.g. agent
// docs about color output — would otherwise trip the ANSI fast path, find
// nothing to style, and render as a plain `<pre>` instead of going through
// Shiki for highlighting. Output whose only sequence is a hyperlink counts:
// it has no color code, and skipping it here leaks the escape bytes.
const HAS_ANSI = /\[[\d;?]*[a-zA-Z]|\][0-9]+(?:;[^\x07]*)?(?:\\|\x07)/;

export function hasAnsi(text: string): boolean {
  return HAS_ANSI.test(text);
}

export function stripAnsi(text: string): string {
  return text.replace(ANY_CSI, "");
}

/** Collapse `\r` repaints: within each `\n`-separated line, drop
 *  everything before the last `\r` so progress bars show their
 *  final state instead of a concatenated history. CRLF line endings
 *  are preserved (a bare `\r` immediately before `\n` carries no
 *  redraw payload, and stripping it would corrupt Windows-emitted
 *  output). */
export function collapseCarriageReturns(text: string): string {
  if (text.indexOf("\r") < 0) return text;
  return text
    .split("\n")
    .map((line) => {
      // Strip a trailing `\r` (the leftover half of `\r\n`) before
      // looking for redraw markers, then re-attach if no redraw was
      // present so multi-line CRLF text round-trips unchanged.
      const hadCrlf = line.endsWith("\r");
      const body = hadCrlf ? line.slice(0, -1) : line;
      const idx = body.lastIndexOf("\r");
      const collapsed = idx >= 0 ? body.slice(idx + 1) : body;
      return hadCrlf ? `${collapsed}\r` : collapsed;
    })
    .join("\n");
}

/** Standard ANSI 16-color palette (VS Code dark+ approximation). */
const FG: Record<number, string> = {
  30: "#000000",
  31: "#cd3131",
  32: "#0dbc79",
  33: "#e5e510",
  34: "#2472c8",
  35: "#bc3fbc",
  36: "#11a8cd",
  37: "#e5e5e5",
  90: "#666666",
  91: "#f14c4c",
  92: "#23d18b",
  93: "#f5f543",
  94: "#3b8eea",
  95: "#d670d6",
  96: "#29b8db",
  97: "#ffffff",
};
const BG: Record<number, string> = {
  40: "#000000",
  41: "#cd3131",
  42: "#0dbc79",
  43: "#e5e510",
  44: "#2472c8",
  45: "#bc3fbc",
  46: "#11a8cd",
  47: "#e5e5e5",
  100: "#666666",
  101: "#f14c4c",
  102: "#23d18b",
  103: "#f5f543",
  104: "#3b8eea",
  105: "#d670d6",
  106: "#29b8db",
  107: "#ffffff",
};

/** xterm 256-color palette → CSS color. */
function palette256(n: number): string {
  if (n < 16) {
    const ordered = [
      FG[30],
      FG[31],
      FG[32],
      FG[33],
      FG[34],
      FG[35],
      FG[36],
      FG[37],
      FG[90],
      FG[91],
      FG[92],
      FG[93],
      FG[94],
      FG[95],
      FG[96],
      FG[97],
    ];
    return ordered[n] ?? "#888888";
  }
  if (n < 232) {
    const i = n - 16;
    const r = Math.floor(i / 36) * 51;
    const g = Math.floor((i % 36) / 6) * 51;
    const b = (i % 6) * 51;
    return `rgb(${r}, ${g}, ${b})`;
  }
  const v = (n - 232) * 10 + 8;
  return `rgb(${v}, ${v}, ${v})`;
}

function applySgr(style: AnsiStyle, params: number[]): AnsiStyle {
  // ESC[m / ESC[0m → full reset. Treat empty params as 0.
  if (params.length === 0) return {};
  const next: AnsiStyle = { ...style };
  let i = 0;
  while (i < params.length) {
    const c = params[i];
    if (c === 0) {
      // Reset all
      for (const k of Object.keys(next) as (keyof AnsiStyle)[]) {
        delete next[k];
      }
      i++;
    } else if (c === 1) {
      next.bold = true;
      i++;
    } else if (c === 2) {
      next.dim = true;
      i++;
    } else if (c === 3) {
      next.italic = true;
      i++;
    } else if (c === 4) {
      next.underline = true;
      i++;
    } else if (c === 7) {
      next.inverse = true;
      i++;
    } else if (c === 22) {
      delete next.bold;
      delete next.dim;
      i++;
    } else if (c === 23) {
      delete next.italic;
      i++;
    } else if (c === 24) {
      delete next.underline;
      i++;
    } else if (c === 27) {
      delete next.inverse;
      i++;
    } else if (c === 39) {
      delete next.fg;
      i++;
    } else if (c === 49) {
      delete next.bg;
      i++;
    } else if (c !== undefined && FG[c]) {
      next.fg = FG[c];
      i++;
    } else if (c !== undefined && BG[c]) {
      next.bg = BG[c];
      i++;
    } else if (c === 38 || c === 48) {
      // Extended color: 38;5;n (256-color) or 38;2;r;g;b (truecolor).
      const target: "fg" | "bg" = c === 38 ? "fg" : "bg";
      const mode = params[i + 1];
      if (mode === 5) {
        next[target] = palette256(params[i + 2] ?? 0);
        i += 3;
      } else if (mode === 2) {
        next[target] = `rgb(${params[i + 2] ?? 0}, ${params[i + 3] ?? 0}, ${params[i + 4] ?? 0})`;
        i += 5;
      } else {
        i++;
      }
    } else {
      // Unknown / unsupported (e.g. 53 overline) — skip.
      i++;
    }
  }
  return next;
}

/** Parse `text` into styled segments, starting from the escape state
 *  `initial` leaves in effect and reporting the state left at the end.
 *  This is the resumable core behind [`parseAnsi`]: a color or an open
 *  hyperlink legitimately spans lines, so a per-line parse cache must
 *  thread both through explicitly. */
export function parseAnsiFrom(text: string, initial: AnsiState): { segs: AnsiSegment[]; exit: AnsiState } {
  const clean = collapseCarriageReturns(text);
  const segs: AnsiSegment[] = [];
  let last = 0;
  let style: AnsiStyle = { ...initial.style };
  let url = initial.url;
  // A dropped sequence (cursor movement, a title set) leaves the state
  // untouched, so the text on either side of it is one span rather than two.
  let restyled = true;
  const emit = (chunk: string) => {
    if (chunk.length === 0) return;
    const prev = segs[segs.length - 1];
    if (!restyled && prev) {
      prev.text += chunk;
      return;
    }
    segs.push({ text: chunk, style: { ...style }, url });
    restyled = false;
  };
  for (const m of clean.matchAll(TOKEN)) {
    const idx = m.index ?? 0;
    emit(clean.slice(last, idx));
    last = idx + m[0].length;
    if (m[2] !== undefined) {
      // CSI: SGR restyles, every other final byte is dropped.
      if (m[2] !== "m") continue;
      const raw = m[1] ?? "";
      style = applySgr(style, raw === "" ? [] : raw.split(";").map((n) => Number(n)));
      restyled = true;
      continue;
    }
    if (m[3] !== "8") continue;
    const payload = m[4] ?? "";
    const sep = payload.indexOf(";");
    // OSC 8 carries `params;URI`. An empty URI closes the link; a second
    // open before a close (malformed input) retargets rather than nests.
    // An open with no close runs to the end of the parsed text, so a frame
    // cut mid-link still shows its visible text.
    url = (sep >= 0 ? payload.slice(sep + 1) : "") || undefined;
    restyled = true;
  }
  emit(clean.slice(last));
  return { segs, exit: { style, url } };
}

/** Parse a string with ANSI escape sequences into styled segments. */
export function parseAnsi(text: string): AnsiSegment[] {
  return parseAnsiFrom(text, { style: {} }).segs;
}

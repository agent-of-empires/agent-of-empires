/* eslint-disable no-control-regex -- this file's whole job is to match ESC sequences */
// ANSI SGR parser for Bash tool output.
//
// claude-agent-acp forwards `\x1b[...m` color escapes from commands
// like `git status --color=always` and `gls --color=always`. Shiki's
// bash grammar treats them as raw text, so the user sees literal
// `[01;34m` noise unless we render them ourselves.
//
// We:
//   1. Collapse `\r` (carriage-return repaints) so progress bars don't
//      flatten into one concatenated line.
//   2. Strip non-SGR CSI sequences (cursor movement, line erase, etc.)
//      that would otherwise leak through as garbage characters.
//   3. Walk the remaining SGR sequences (`\x1b[<n;n;…>m`) and emit
//      typed segments the React layer can style.

// Any CSI sequence: ESC [ params final-byte (any letter).
const ANY_CSI = /\[[\d;?]*[a-zA-Z]/g;
// SGR specifically: same shape, terminated by `m`.
const SGR = /\[([\d;]*)m/g;
// CSI sequences other than SGR — anything ending in a letter that
// isn't `m`. We match the full sequence so ANY_CSI followed by a
// negative-set replace would risk eating SGR; instead use a
// non-`m`-terminator pattern.
const NON_SGR_CSI = /\[[\d;?]*[a-ln-zA-LN-Z]/g;

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
}

// Match a real CSI sequence (`ESC [ params final-byte`), not just the
// `ESC [` prefix. A markdown blob that quotes the literal characters
// "\x1b[" — e.g. agent docs about color output — would otherwise trip
// the ANSI fast path, find no SGR, and render as a plain `<pre>`
// instead of going through Shiki for highlighting.
const HAS_ANSI = /\x1b\[[\d;?]*[a-zA-Z]/;

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
        next[target] =
          `rgb(${params[i + 2] ?? 0}, ${params[i + 3] ?? 0}, ${params[i + 4] ?? 0})`;
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

/** Parse a string with ANSI SGR codes into styled segments. Non-SGR
 *  CSI sequences and `\r` repaints are stripped/collapsed first. */
export function parseAnsi(text: string): AnsiSegment[] {
  const cleaned = collapseCarriageReturns(text).replace(NON_SGR_CSI, "");
  const segs: AnsiSegment[] = [];
  let last = 0;
  let cur: AnsiStyle = {};
  for (const m of cleaned.matchAll(SGR)) {
    const idx = m.index ?? 0;
    if (idx > last) {
      segs.push({ text: cleaned.slice(last, idx), style: { ...cur } });
    }
    const raw = m[1] ?? "";
    const params = raw === "" ? [] : raw.split(";").map((s) => Number(s));
    cur = applySgr(cur, params);
    last = idx + m[0].length;
  }
  if (last < cleaned.length) {
    segs.push({ text: cleaned.slice(last), style: { ...cur } });
  }
  return segs.filter((s) => s.text.length > 0);
}

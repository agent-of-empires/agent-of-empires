/** Terminal byte sequences for keys the browser does not type as text. */

export interface KeyboardLayoutReader {
  get: (code: string) => string | undefined;
}

interface TerminalKeyLike {
  key: string;
  shiftKey: boolean;
  altKey: boolean;
  ctrlKey: boolean;
  metaKey: boolean;
}

/** The Meta character for a printable Alt chord, or null when the chord is not printable. */
export function altPrintableMetaKey(
  e: { key: string; code: string; shiftKey: boolean },
  layoutMap: KeyboardLayoutReader | null,
): string | null {
  if (e.key === "Dead") return null;
  const code = e.key.length === 1 ? e.key.charCodeAt(0) : 0;
  if (code >= 0x20 && code <= 0x7e) return e.key;
  // macOS Option+letter composes a symbol (Option+V is "√"): recover the letter from the logical
  // layout map when available (AZERTY KeyQ is "a"), else from the physical key. Letters only.
  if (!/^Key[A-Z]$/.test(e.code)) return null;
  const mapped = layoutMap?.get(e.code);
  const letter = mapped && mapped.length === 1 && /^[a-z]$/i.test(mapped) ? mapped.toLowerCase() : e.code.slice(3);
  return e.shiftKey ? letter.toUpperCase() : letter.toLowerCase();
}

/** CSI parameter and final byte per navigation key; letter finals omit the parameter when unmodified. */
const NAVIGATION: Record<string, [string, string]> = {
  ArrowUp: ["1", "A"],
  ArrowDown: ["1", "B"],
  ArrowRight: ["1", "C"],
  ArrowLeft: ["1", "D"],
  Home: ["1", "H"],
  End: ["1", "F"],
  Insert: ["2", "~"],
  Delete: ["3", "~"],
  PageUp: ["5", "~"],
  PageDown: ["6", "~"],
};

function navigationKeySequence(e: TerminalKeyLike): string | null {
  const nav = NAVIGATION[e.key];
  if (e.metaKey || !nav) return null;
  const [param, final] = nav;
  const modifier = 1 + Number(e.shiftKey) + 2 * Number(e.altKey) + 4 * Number(e.ctrlKey);
  if (modifier !== 1) return `\x1b[${param};${modifier}${final}`;
  return final === "~" ? `\x1b[${param}~` : `\x1b[${final}`;
}

export function specialKeySequence(e: TerminalKeyLike): string | null {
  switch (e.key) {
    case "Enter":
      // Shift/Ctrl+Enter inserts a soft newline (ESC CR, as Alt+Enter) instead of submitting.
      return (e.shiftKey || e.ctrlKey) && !e.altKey && !e.metaKey ? "\x1b\r" : "\r";
    case "Backspace":
      return e.altKey && !e.ctrlKey && !e.metaKey ? "\x1b\x7f" : "\x7f";
    case "Tab":
      return e.shiftKey ? "\x1b[Z" : "\t";
    case "Escape":
      return "\x1b";
    default:
      return navigationKeySequence(e);
  }
}

/** The ^A..^Z control code for a letter, or null for anything else. */
export function controlCode(key: string): string | null {
  const code = key.toUpperCase().charCodeAt(0);
  return key.length === 1 && code >= 65 && code <= 90 ? String.fromCharCode(code - 64) : null;
}

/** The word under the caret: the non-whitespace run ending `run + typed`. Uncapped, so the IME's word always prefixes it. */
export function plainRunAfter(run: string, typed: string): string {
  return /\S*$/.exec(run + typed)?.[0] ?? "";
}

/** Drops one code point, so backspacing an emoji leaves no half surrogate in the run. */
export function dropLastCodePoint(run: string): string {
  return Array.from(run).slice(0, -1).join("");
}

/** Backslash-escapes whitespace and backslashes, as terminal drag-and-drop does. */
export function escapePastePath(p: string): string {
  return p.replace(/[\\ \t]/g, (c) => `\\${c}`);
}

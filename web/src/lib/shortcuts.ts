// Single source of truth for the dashboard's keyboard shortcuts.
//
// Three consumers read from SHORTCUTS so they cannot drift apart:
//   - useKeyboardShortcuts (the keydown handler, via matchShortcut)
//   - HelpOverlay (the `?` overlay list and footer)
//   - tourSteps / TourRunner (the first-run tutorial hints, by id)
//
// Each entry separates two concerns that are deliberately unrelated:
//   - `chord`: how the shortcut is *displayed* (e.g. "⌘⌥B"), driven by the
//     mod/alt/shift booleans plus a base label.
//   - `trigger`: how the keydown is *matched* (e.g. `e.code === "KeyB"`),
//     which often differs from the display because of layout quirks (Option+B
//     on Mac yields "∫", backtick can live behind a modifier).
//
// The drift guard in shortcuts.test.ts locks the binding behavior, the exact
// rendered label strings, and the tour's id references.

export const IS_MAC =
  typeof navigator !== "undefined" &&
  /Mac|iPhone|iPad|iPod/.test(navigator.platform);

export interface ShortcutActions {
  onNew: () => void;
  /** Fast-path: opens the wizard pre-configured for a scratch session
   *  and jumped to the Review step so Cmd+Enter immediately creates it. */
  onNewScratch: () => void;
  onDiff: () => void;
  onEscape: () => void;
  onHelp: () => void;
  onSettings: () => void;
  onPalette: () => void;
  onToggleSidebar: () => void;
  onToggleRightPanel: () => void;
  onToggleTerminalFocus: () => void;
}

export type ShortcutId =
  | "palette"
  | "sidebar"
  | "rightPanel"
  | "terminalFocus"
  | "new"
  | "newScratch"
  | "diff"
  | "settings"
  | "escape"
  | "help";

/** The display model: which modifier glyphs to show, plus the base label. */
export interface ShortcutChord {
  mod?: boolean;
  alt?: boolean;
  shift?: boolean;
  base: string;
}

interface ShortcutTrigger {
  /**
   * `global` shortcuts fire even when an input/textarea/terminal is focused.
   * `textless` shortcuts fire only when no input is focused and no
   * meta/ctrl/alt is held (shift is allowed, e.g. Shift+/ to type "?").
   */
  scope: "global" | "textless";
  /** Logical mod: metaKey on Mac, metaKey OR ctrlKey elsewhere. */
  mod?: boolean;
  shift?: boolean;
  alt?: boolean;
  /** Layout-stable physical key. Preferred for letter combos. */
  code?: string;
  /** Logical key. Used when `code` is not appropriate (Escape, "?"). */
  key?: string;
  keyCaseInsensitive?: boolean;
  preventDefault: boolean;
  stopPropagation: boolean;
}

export interface ShortcutDef {
  id: ShortcutId;
  action: keyof ShortcutActions;
  description: string;
  chord: ShortcutChord;
  trigger: ShortcutTrigger;
}

// Ordered to match the help overlay's display order. Match predicates are
// mutually exclusive (see the exclusivity guard in shortcuts.test.ts), so this
// order does not affect which shortcut a keydown resolves to; it is purely the
// rendered order of the overlay.
export const SHORTCUTS: readonly ShortcutDef[] = [
  {
    id: "palette",
    action: "onPalette",
    description: "Open command palette",
    chord: { mod: true, base: "K" },
    // Works everywhere. Uses e.key (case-insensitive) since "k" is layout-stable.
    trigger: {
      scope: "global",
      mod: true,
      shift: false,
      alt: false,
      key: "k",
      keyCaseInsensitive: true,
      preventDefault: true,
      stopPropagation: true,
    },
  },
  {
    id: "sidebar",
    action: "onToggleSidebar",
    description: "Toggle left sidebar",
    chord: { mod: true, base: "B" },
    // e.code because Option+B on Mac produces "∫" instead of "b".
    trigger: {
      scope: "global",
      mod: true,
      shift: false,
      alt: false,
      code: "KeyB",
      preventDefault: true,
      stopPropagation: true,
    },
  },
  {
    id: "rightPanel",
    action: "onToggleRightPanel",
    description: "Toggle right panel",
    chord: { mod: true, alt: true, base: "B" },
    trigger: {
      scope: "global",
      mod: true,
      shift: false,
      alt: true,
      code: "KeyB",
      preventDefault: true,
      stopPropagation: true,
    },
  },
  {
    id: "terminalFocus",
    action: "onToggleTerminalFocus",
    description: "Toggle agent / shell terminal focus",
    chord: { mod: true, base: "`" },
    // No stopPropagation: preventDefault alone suppresses the browser's own
    // Cmd+` window cycling, and we don't want to shadow other doc-level
    // listeners. e.code so layouts with backtick behind a modifier still match.
    trigger: {
      scope: "global",
      mod: true,
      shift: false,
      alt: false,
      code: "Backquote",
      preventDefault: true,
      stopPropagation: false,
    },
  },
  {
    id: "new",
    action: "onNew",
    description: "New session",
    chord: { base: "n" },
    trigger: {
      scope: "textless",
      key: "n",
      preventDefault: true,
      stopPropagation: false,
    },
  },
  {
    id: "newScratch",
    action: "onNewScratch",
    description: "New scratch session",
    chord: { mod: true, shift: true, base: "N" },
    // Works regardless of focus so it fires from the terminal pane too.
    trigger: {
      scope: "global",
      mod: true,
      shift: true,
      alt: false,
      code: "KeyN",
      preventDefault: true,
      stopPropagation: true,
    },
  },
  {
    id: "diff",
    action: "onDiff",
    description: "Toggle diff panel",
    chord: { base: "D" },
    trigger: {
      scope: "textless",
      key: "D",
      preventDefault: true,
      stopPropagation: false,
    },
  },
  {
    id: "settings",
    action: "onSettings",
    description: "Toggle settings",
    chord: { base: "s" },
    trigger: {
      scope: "textless",
      key: "s",
      preventDefault: true,
      stopPropagation: false,
    },
  },
  {
    id: "escape",
    action: "onEscape",
    description: "Close dialog",
    chord: { base: "Esc" },
    // Fires regardless of focus and regardless of modifiers.
    trigger: {
      scope: "global",
      key: "Escape",
      preventDefault: false,
      stopPropagation: false,
    },
  },
  {
    id: "help",
    action: "onHelp",
    description: "Toggle this help",
    chord: { base: "?" },
    trigger: {
      scope: "textless",
      key: "?",
      preventDefault: true,
      stopPropagation: false,
    },
  },
] as const;

export const SHORTCUTS_BY_ID: Record<ShortcutId, ShortcutDef> =
  Object.fromEntries(SHORTCUTS.map((s) => [s.id, s])) as Record<
    ShortcutId,
    ShortcutDef
  >;

function modifierGlyphs(chord: ShortcutChord, mac: boolean): string[] {
  const parts: string[] = [];
  if (chord.mod) parts.push(mac ? "⌘" : "Ctrl");
  if (chord.alt) parts.push(mac ? "⌥" : "Alt");
  if (chord.shift) parts.push(mac ? "⇧" : "Shift");
  return parts;
}

/**
 * Render a chord for one platform. Mac always concatenates glyphs with no
 * separator (e.g. "⌘⌥B"); other platforms join with `separator` (the help
 * overlay passes "" for "CtrlAltB", the tour passes "+" for "Ctrl+Alt+B").
 */
export function formatShortcut(
  chord: ShortcutChord,
  { mac, separator = "" }: { mac: boolean; separator?: string },
): string {
  const parts = modifierGlyphs(chord, mac);
  parts.push(chord.base);
  return mac ? parts.join("") : parts.join(separator);
}

/** The help overlay form: current platform only, no separator. */
export function formatHelpShortcut(chord: ShortcutChord, mac: boolean): string {
  return formatShortcut(chord, { mac, separator: "" });
}

/**
 * The tour form: both platforms, joined with " / " (e.g. "⌘K / Ctrl+K").
 * Modifier-less chords are identical across platforms, so they render once.
 */
export function formatTourShortcut(chord: ShortcutChord): string {
  const macForm = formatShortcut(chord, { mac: true });
  const otherForm = formatShortcut(chord, { mac: false, separator: "+" });
  return macForm === otherForm ? macForm : `${macForm} / ${otherForm}`;
}

/** The subset of a KeyboardEvent the matcher reads; lets tests pass plain objects. */
export type ShortcutKeyEvent = Pick<
  KeyboardEvent,
  "key" | "code" | "metaKey" | "ctrlKey" | "altKey" | "shiftKey"
>;

export interface MatchedShortcut {
  shortcut: ShortcutDef;
  preventDefault: boolean;
  stopPropagation: boolean;
}

function globalMatches(
  e: ShortcutKeyEvent,
  t: ShortcutTrigger,
  mod: boolean,
): boolean {
  if (t.mod !== undefined && t.mod !== mod) return false;
  if (t.shift !== undefined && t.shift !== e.shiftKey) return false;
  if (t.alt !== undefined && t.alt !== e.altKey) return false;
  if (t.code !== undefined) return e.code === t.code;
  if (t.key !== undefined) {
    return t.keyCaseInsensitive
      ? e.key.toLowerCase() === t.key.toLowerCase()
      : e.key === t.key;
  }
  return false;
}

function toMatched(shortcut: ShortcutDef): MatchedShortcut {
  return {
    shortcut,
    preventDefault: shortcut.trigger.preventDefault,
    stopPropagation: shortcut.trigger.stopPropagation,
  };
}

/**
 * Resolve a keydown to a shortcut. Global shortcuts are evaluated first and
 * fire regardless of focus; single-key shortcuts fire only when not typing and
 * no meta/ctrl/alt is held. Pure (no DOM access) so it is unit-testable.
 */
export function matchShortcut(
  e: ShortcutKeyEvent,
  { mac, isInput }: { mac: boolean; isInput: boolean },
): MatchedShortcut | null {
  const mod = mac ? e.metaKey : e.metaKey || e.ctrlKey;

  for (const s of SHORTCUTS) {
    if (s.trigger.scope !== "global") continue;
    if (globalMatches(e, s.trigger, mod)) return toMatched(s);
  }

  if (isInput) return null;
  if (e.metaKey || e.ctrlKey || e.altKey) return null;

  for (const s of SHORTCUTS) {
    if (s.trigger.scope !== "textless") continue;
    if (e.key === s.trigger.key) return toMatched(s);
  }
  return null;
}

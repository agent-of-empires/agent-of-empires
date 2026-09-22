// Single source of truth for keyboard shortcuts (keydown handler, help overlay, tour hints). `chord` is display; `trigger` is matching, which differs for layout quirks.

export const IS_MAC = typeof navigator !== "undefined" && /Mac|iPhone|iPad|iPod/.test(navigator.platform);

export interface ShortcutActions {
  onNew: () => void;
  onJumpToAttention: () => void;
  /** Opens the wizard on the Review step for a scratch session. */
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
  | "jumpAttention"
  | "diff"
  | "settings"
  | "escape"
  | "help";

export interface ShortcutChord {
  mod?: boolean;
  alt?: boolean;
  shift?: boolean;
  base: string;
}

interface ShortcutTrigger {
  /** `global` fires even in inputs; `textless` fires only outside inputs with no meta/ctrl/alt. */
  scope: "global" | "textless";
  /** metaKey on Mac, metaKey or ctrlKey elsewhere. */
  mod?: boolean;
  shift?: boolean;
  alt?: boolean;
  code?: string;
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

// Overlay display order; matches are mutually exclusive, so order never decides a keydown.
export const SHORTCUTS: readonly ShortcutDef[] = [
  {
    id: "palette",
    action: "onPalette",
    description: "Open command palette",
    chord: { mod: true, base: "K" },
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
    // e.code because Option+B on Mac produces "∫".
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
    // e.code so layouts with backtick behind a modifier still match.
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
    id: "jumpAttention",
    action: "onJumpToAttention",
    description: "Jump to next session needing attention",
    chord: { base: "a" },
    trigger: {
      scope: "textless",
      key: "a",
      preventDefault: true,
      stopPropagation: false,
    },
  },
  {
    id: "diff",
    action: "onDiff",
    description: "Toggle diff pane",
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

export const SHORTCUTS_BY_ID: Record<ShortcutId, ShortcutDef> = Object.fromEntries(
  SHORTCUTS.map((s) => [s.id, s]),
) as Record<ShortcutId, ShortcutDef>;

function modifierGlyphs(chord: ShortcutChord, mac: boolean): string[] {
  const parts: string[] = [];
  if (chord.mod) parts.push(mac ? "⌘" : "Ctrl");
  if (chord.alt) parts.push(mac ? "⌥" : "Alt");
  if (chord.shift) parts.push(mac ? "⇧" : "Shift");
  return parts;
}

/** Mac concatenates glyphs; other platforms join with `separator`. */
export function formatShortcut(
  chord: ShortcutChord,
  { mac, separator = "" }: { mac: boolean; separator?: string },
): string {
  const parts = modifierGlyphs(chord, mac);
  parts.push(chord.base);
  return mac ? parts.join("") : parts.join(separator);
}

export function formatHelpShortcut(chord: ShortcutChord, mac: boolean): string {
  return formatShortcut(chord, { mac, separator: "" });
}

/** Both platforms, e.g. "⌘K / Ctrl+K"; modifier-less chords render once. */
export function formatTourShortcut(chord: ShortcutChord): string {
  const macForm = formatShortcut(chord, { mac: true });
  const otherForm = formatShortcut(chord, { mac: false, separator: "+" });
  return macForm === otherForm ? macForm : `${macForm} / ${otherForm}`;
}

export type ShortcutKeyEvent = Pick<KeyboardEvent, "key" | "code" | "metaKey" | "ctrlKey" | "altKey" | "shiftKey">;

export interface MatchedShortcut {
  shortcut: ShortcutDef;
  preventDefault: boolean;
  stopPropagation: boolean;
}

function globalMatches(e: ShortcutKeyEvent, t: ShortcutTrigger, mod: boolean): boolean {
  if (t.mod !== undefined && t.mod !== mod) return false;
  if (t.shift !== undefined && t.shift !== e.shiftKey) return false;
  if (t.alt !== undefined && t.alt !== e.altKey) return false;
  if (t.code !== undefined) return e.code === t.code;
  if (t.key !== undefined) {
    return t.keyCaseInsensitive ? e.key.toLowerCase() === t.key.toLowerCase() : e.key === t.key;
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

/** Global shortcuts first; single-key ones only when not typing. */
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

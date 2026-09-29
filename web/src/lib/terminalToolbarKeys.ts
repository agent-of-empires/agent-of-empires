export type ToolbarKeyId =
  | "esc"
  | "tab"
  | "shift-tab"
  | "ctrl"
  | "backspace"
  | "enter"
  | "ctrl-c"
  | "ctrl-o"
  | "page-up"
  | "page-down"
  | "home"
  | "end"
  | "paste"
  | "compose";

export interface ToolbarKeySpec {
  id: ToolbarKeyId;
  label: string;
  /** Accessible name. */
  name: string;
  /** Bytes for a plain key; Ctrl, Paste, and Compose act instead of sending. */
  data?: string;
  repeat?: boolean;
}

// Shift+Tab cycles Claude Code's mode and Ctrl+O expands its transcript.
export const TOOLBAR_KEY_CATALOG: readonly ToolbarKeySpec[] = [
  { id: "esc", label: "Esc", name: "Escape", data: "\x1b" },
  { id: "tab", label: "Tab", name: "Tab", data: "\t" },
  { id: "shift-tab", label: "⇧Tab", name: "Shift+Tab", data: "\x1b[Z" },
  { id: "ctrl", label: "Ctrl", name: "Ctrl" },
  { id: "backspace", label: "⌫", name: "Backspace", data: "\x7f", repeat: true },
  { id: "enter", label: "⏎", name: "Enter", data: "\r" },
  { id: "ctrl-c", label: "^C", name: "Ctrl+C interrupt", data: "\x03" },
  { id: "ctrl-o", label: "^O", name: "Ctrl+O", data: "\x0f" },
  { id: "page-up", label: "PgUp", name: "Page up", data: "\x1b[5~", repeat: true },
  { id: "page-down", label: "PgDn", name: "Page down", data: "\x1b[6~", repeat: true },
  { id: "home", label: "Home", name: "Home", data: "\x1b[H" },
  { id: "end", label: "End", name: "End", data: "\x1b[F" },
  { id: "paste", label: "Paste", name: "Paste from clipboard" },
  { id: "compose", label: "Compose", name: "Compose" },
];

export const DEFAULT_TOOLBAR_KEYS: readonly ToolbarKeyId[] = ["esc", "tab", "ctrl", "paste", "compose"];

/** Eight keys stay at least 40px wide on a 360px phone, so the row never scrolls. */
export const MAX_TOOLBAR_KEYS = 8;

const SPECS = new Map(TOOLBAR_KEY_CATALOG.map((spec) => [spec.id, spec]));

export function toolbarKeySpec(id: ToolbarKeyId): ToolbarKeySpec {
  return SPECS.get(id)!;
}

/** Known ids in stored order, without duplicates, capped; anything but an array is the default row. */
export function normalizeToolbarKeys(value: unknown): ToolbarKeyId[] {
  if (!Array.isArray(value)) return [...DEFAULT_TOOLBAR_KEYS];
  const ids = value.filter((id): id is ToolbarKeyId => typeof id === "string" && SPECS.has(id as ToolbarKeyId));
  return [...new Set(ids)].slice(0, MAX_TOOLBAR_KEYS);
}

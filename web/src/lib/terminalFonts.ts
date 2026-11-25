// Terminal font choices. The value is synced across devices, so a system font picked on one device may be absent
// on another; the CSS stack then falls back to the bundled default.

import { DEFAULT_LINE_RATIO } from "../components/live-terminal/rowHeight";

/** Stored value for the default, Geist Mono through `--font-mono`. */
export const DEFAULT_TERMINAL_FONT = "";
export const DEFAULT_TERMINAL_FONT_NAME = "Geist Mono";

/** Self-hosted, so always available. Ratios are each font's ascent plus descent over its em, as Ghostty sizes cells. */
const BUNDLED_LINE_RATIOS: Record<string, number> = {
  "JetBrains Mono": 1.32,
};
export const BUNDLED_TERMINAL_FONTS = Object.keys(BUNDLED_LINE_RATIOS);

/** Offered only when detected on this device. Nerd Fonts are listed under their installed family names. */
export const SYSTEM_TERMINAL_FONTS = [
  "SF Mono",
  "Menlo",
  "Monaco",
  "Consolas",
  "Cascadia Code",
  "Cascadia Mono",
  "Ubuntu Mono",
  "DejaVu Sans Mono",
  "Liberation Mono",
  "Noto Sans Mono",
  "Fira Code",
  "Source Code Pro",
  "IBM Plex Mono",
  "Hack",
  "Iosevka",
  "Iosevka Term",
  "Berkeley Mono",
  "Monaspace Neon",
  "Commit Mono",
  "Maple Mono",
  "Victor Mono",
  "Roboto Mono",
  "Inconsolata",
  "Anonymous Pro",
  "Courier New",
  "JetBrainsMono Nerd Font",
  "FiraCode Nerd Font",
  "MesloLGS NF",
  "MesloLGS Nerd Font",
  "MesloLGL Nerd Font",
  "Hack Nerd Font",
  "CaskaydiaCove Nerd Font",
  "SauceCodePro Nerd Font",
];

const KNOWN = new Map(
  [DEFAULT_TERMINAL_FONT_NAME, ...BUNDLED_TERMINAL_FONTS, ...SYSTEM_TERMINAL_FONTS].map((name) => [
    name.toLowerCase(),
    name === DEFAULT_TERMINAL_FONT_NAME ? DEFAULT_TERMINAL_FONT : name,
  ]),
);

/** Map a stored value, possibly free text from the old text input, to a listed font or the default. */
export function normalizeTerminalFontFamily(value: unknown): string {
  if (typeof value !== "string") return DEFAULT_TERMINAL_FONT;
  const key = value.replace(/["']/g, "").trim().toLowerCase();
  return KNOWN.get(key) ?? DEFAULT_TERMINAL_FONT;
}

/** CSS `font-family` for a normalized value; undefined inherits `--font-mono`. */
export function terminalFontStack(family: string): string | undefined {
  return family ? `"${family}", var(--font-mono)` : undefined;
}

export function terminalLineRatio(family: string): number {
  return BUNDLED_LINE_RATIOS[family] ?? DEFAULT_LINE_RATIO;
}

export interface TerminalFontChoices {
  /** Listed system fonts detected on this device, in list order. */
  installed: string[];
  /** The saved system font when this device lacks it; the terminal renders the default instead. */
  missing: string | null;
}

export function terminalFontChoices(saved: string, detected: readonly string[]): TerminalFontChoices {
  const found = new Set(detected);
  const installed = SYSTEM_TERMINAL_FONTS.filter((name) => found.has(name));
  const isSystem = SYSTEM_TERMINAL_FONTS.includes(saved);
  return { installed, missing: isSystem && !found.has(saved) ? saved : null };
}

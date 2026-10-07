// Resolved theme types; applies the server's CSS variable projection to the root and caches it for the next cold paint.

export type ThemeAppearance = "dark" | "light";

export type ResolvedThemeSource = "builtin" | "custom" | "fallback";

export interface CssVarProjection {
  cssVars: Record<string, string>;
}

export interface ResolvedTheme {
  name: string;
  source: ResolvedThemeSource;
  appearance: ThemeAppearance;
  web: CssVarProjection;
  terminal: CssVarProjection;
  syntax: { shikiTheme: string };
}

import { safeGetItem, safeSetItem } from "./safeStorage";

const STORAGE_KEY = "aoe-resolved-theme";

export function readCachedResolvedTheme(): ResolvedTheme | null {
  const raw = safeGetItem(STORAGE_KEY);
  if (!raw) return null;
  try {
    return JSON.parse(raw) as ResolvedTheme;
  } catch {
    return null;
  }
}

function writeCachedResolvedTheme(theme: ResolvedTheme): void {
  safeSetItem(STORAGE_KEY, JSON.stringify(theme));
}

// setProperty needs no CSP allowance and repaints Tailwind utilities immediately.
export function applyResolvedTheme(theme: ResolvedTheme): void {
  const root = document.documentElement;
  for (const [name, value] of Object.entries(theme.web.cssVars)) {
    root.style.setProperty(name, value);
  }
  for (const [name, value] of Object.entries(theme.terminal.cssVars)) {
    root.style.setProperty(name, value);
  }
  root.dataset.theme = theme.name;
  root.dataset.themeAppearance = theme.appearance;
  root.style.colorScheme = theme.appearance;
  writeCachedResolvedTheme(theme);
}

// Lets Shiki call sites re-render on theme changes without a context.
export const THEME_CHANGED_EVENT = "aoe:theme-changed";

export function dispatchThemeChanged(theme: ResolvedTheme): void {
  window.dispatchEvent(new CustomEvent<ResolvedTheme>(THEME_CHANGED_EVENT, { detail: theme }));
}

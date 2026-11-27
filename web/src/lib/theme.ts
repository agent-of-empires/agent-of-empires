// The resolved theme store: applies the server's CSS variable projection to <html>, caches it for the next cold paint, and notifies JS consumers.

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

// Also read by public/theme-bootstrap.js, and its `storage` event syncs other windows.
export const THEME_STORAGE_KEY = "aoe-resolved-theme";

export function readCachedResolvedTheme(): ResolvedTheme | null {
  return parseTheme(safeGetItem(THEME_STORAGE_KEY));
}

export function parseTheme(raw: string | null): ResolvedTheme | null {
  if (!raw) return null;
  try {
    return JSON.parse(raw) as ResolvedTheme;
  } catch {
    return null;
  }
}

// The one source of truth for the active theme: <html> carries it as CSS variables, and JS
// consumers (Shiki, plugin frames) subscribe here instead of reading the cache themselves.
let current: ResolvedTheme | null | undefined;
const listeners = new Set<() => void>();

export function getResolvedTheme(): ResolvedTheme | null {
  if (current === undefined) current = readCachedResolvedTheme();
  return current;
}

export function subscribeTheme(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Paints `theme` on <html> and notifies subscribers; `persist` is false when another window already cached it. */
export function setResolvedTheme(theme: ResolvedTheme, persist = true): void {
  // setProperty needs no CSP allowance and repaints Tailwind utilities immediately.
  const root = document.documentElement;
  for (const [name, value] of Object.entries({ ...theme.web.cssVars, ...theme.terminal.cssVars })) {
    root.style.setProperty(name, value);
  }
  root.dataset.theme = theme.name;
  root.dataset.themeAppearance = theme.appearance;
  root.style.colorScheme = theme.appearance;
  if (persist) safeSetItem(THEME_STORAGE_KEY, JSON.stringify(theme));
  current = theme;
  for (const listener of listeners) listener();
}

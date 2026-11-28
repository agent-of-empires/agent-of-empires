import type { ResolvedTheme } from "./theme";
import { pickContrastForeground } from "./contrastColor";

// Matches the `--color-status-unread`/`--color-status-waiting` defaults in index.css, used before the first
// resolved-theme fetch lands and for themes that omit an override.
const DEFAULT_UNREAD_BG = "#38bdf8";
const DEFAULT_WAITING_BG = "#fbbf24";

export interface AttentionBadgeColors {
  unreadBg: string;
  unreadFg: string;
  waitingBg: string;
  waitingFg: string;
}

/** Derives the TopBar count badges' unread/waiting accent colors and a WCAG-safe foreground for each from the
 *  resolved theme, so the badges stay legible against any built-in or custom theme's accent. */
export function getAttentionBadgeColors(resolvedTheme: ResolvedTheme | null): AttentionBadgeColors {
  const unreadBg = resolvedTheme?.web.cssVars["--color-status-unread"] ?? DEFAULT_UNREAD_BG;
  const waitingBg = resolvedTheme?.web.cssVars["--color-status-waiting"] ?? DEFAULT_WAITING_BG;
  return {
    unreadBg,
    unreadFg: pickContrastForeground(unreadBg),
    waitingBg,
    waitingFg: pickContrastForeground(waitingBg),
  };
}

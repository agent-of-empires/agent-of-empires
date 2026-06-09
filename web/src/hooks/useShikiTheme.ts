import { useEffect, useState } from "react";
import { readCachedResolvedTheme, THEME_CHANGED_EVENT, type ResolvedTheme } from "../lib/theme";
import { DEFAULT_SHIKI_THEME } from "../lib/highlighter";

export interface ShikiThemeState {
  /** Bundled Shiki theme name to pass to ensureThemeLoaded. */
  theme: string;
  /** Appearance of the active AoE theme; passed to ensureThemeLoaded
   *  so a light AoE theme that names an unbundled Shiki theme falls
   *  back to `github-light` instead of `github-dark`. */
  appearance: "dark" | "light";
}

/** Current Shiki theme + appearance from the resolved theme. Updates
 *  when the user picks a new theme (via THEME_CHANGED_EVENT broadcast
 *  from useResolvedTheme). Components that highlight code should put
 *  both fields in their effect dependency list so blocks re-render
 *  against the new theme. */
export function useShikiTheme(): ShikiThemeState {
  const [state, setState] = useState<ShikiThemeState>(() => {
    const cached = readCachedResolvedTheme();
    return {
      theme: cached?.syntax.shikiTheme ?? DEFAULT_SHIKI_THEME,
      appearance: cached?.appearance ?? "dark",
    };
  });
  useEffect(() => {
    const onChange = (event: Event) => {
      const next = (event as CustomEvent<ResolvedTheme>).detail;
      if (!next) return;
      setState({
        theme: next.syntax.shikiTheme,
        appearance: next.appearance,
      });
    };
    window.addEventListener(THEME_CHANGED_EVENT, onChange);
    return () => {
      window.removeEventListener(THEME_CHANGED_EVENT, onChange);
    };
  }, []);
  return state;
}

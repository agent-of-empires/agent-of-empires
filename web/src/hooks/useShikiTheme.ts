import { useEffect, useState } from "react";
import { readCachedResolvedTheme, THEME_CHANGED_EVENT, type ResolvedTheme } from "../lib/theme";
import { DEFAULT_SHIKI_THEME } from "../lib/snippetHighlighter";
import { listen } from "./domEvents";

export interface ShikiThemeState {
  theme: string;
  appearance: "dark" | "light";
}

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
      setState({ theme: next.syntax.shikiTheme, appearance: next.appearance });
    };
    return listen(onChange, [window, THEME_CHANGED_EVENT]);
  }, []);
  return state;
}

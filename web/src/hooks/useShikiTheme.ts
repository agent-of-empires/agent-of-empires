import { DEFAULT_SHIKI_THEME } from "../lib/shikiDefaults";
import { useCurrentTheme } from "./useResolvedTheme";

export interface ShikiThemeState {
  theme: string;
  appearance: "dark" | "light";
}

export function useShikiTheme(): ShikiThemeState {
  const resolved = useCurrentTheme();
  return {
    theme: resolved?.syntax.shikiTheme ?? DEFAULT_SHIKI_THEME,
    appearance: resolved?.appearance ?? "dark",
  };
}

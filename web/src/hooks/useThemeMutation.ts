import { useCallback, useState } from "react";
import { fetchSettings, updateTheme } from "../lib/api";
import { dispatchThemePickerChanged } from "./useResolvedTheme";

export type ThemeSelectResult = { ok: true } | { ok: false; error: string };

const SAVE_ERROR = "Could not save theme. Please try again.";

// Writes the global theme, then repaints only after the PATCH lands so a failed save never applies.
export function useThemeMutation(): {
  select: (name: string) => Promise<ThemeSelectResult>;
  pending: boolean;
} {
  const [pending, setPending] = useState(false);

  const select = useCallback(async (name: string): Promise<ThemeSelectResult> => {
    setPending(true);
    try {
      const outcome = await updateTheme({ name });
      // A lost response may still have persisted the pick; only a fetch showing it counts as saved.
      const saved =
        outcome === "committed" || (outcome === "uncertain" && (await fetchSettings())?.theme?.name === name);
      if (!saved) return { ok: false, error: SAVE_ERROR };
      dispatchThemePickerChanged(name);
      return { ok: true };
    } catch {
      return { ok: false, error: SAVE_ERROR };
    } finally {
      setPending(false);
    }
  }, []);

  return { select, pending };
}

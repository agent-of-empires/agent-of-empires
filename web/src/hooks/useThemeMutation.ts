import { useCallback, useState } from "react";
import { updateTheme } from "../lib/api";
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
      const ok = await updateTheme({ name });
      if (!ok) return { ok: false, error: SAVE_ERROR };
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

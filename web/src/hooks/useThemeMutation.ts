import { useCallback, useState } from "react";
import { updateTheme } from "../lib/api";
import { claimThemeWrite, confirmThemeWrite, isLatestThemeWrite } from "../lib/themeWrites";
import { dispatchThemePickerChanged } from "./useResolvedTheme";

export type ThemeSelectResult = { ok: true } | { ok: false; error: string };

const MAX_ATTEMPTS = 3;
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
      // A write whose response was lost may still commit, so resend it, a newer stamp winning
      // over any late older one, instead of reading one stale fetch as proof it did not land.
      const claim = claimThemeWrite("name");
      let saved = false;
      for (let attempt = 0; attempt < MAX_ATTEMPTS && !saved && isLatestThemeWrite("name", claim); attempt++) {
        const outcome = await updateTheme({ name });
        if (outcome === "rejected") break;
        saved = outcome === "committed" || (await confirmThemeWrite("name", name));
      }
      saved &&= isLatestThemeWrite("name", claim);
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

import { useCallback, useState } from "react";
import { updateTheme } from "../lib/api";
import { dispatchThemePickerChanged } from "./useResolvedTheme";

export type ThemeSelectResult = { ok: true } | { ok: false; error: string };

const SAVE_ERROR = "Could not save theme. Please try again.";

/**
 * Persist a theme selection and repaint, used by the first-run theme welcome
 * modal. The theme is a global preference, so this writes the global config
 * (PATCH /api/theme), not a profile override; writing it per-profile let a
 * stale override shadow the TUI's global pick and flip the theme on every
 * Settings open/close. Persist-then-paint: the dashboard only repaints after
 * the PATCH lands (via dispatchThemePickerChanged, which routes through
 * useResolvedTheme's single apply path), so a failed save never leaves the
 * applied/cached theme ahead of what is on disk (the #1510 bug class).
 */
export function useThemeMutation(): {
  select: (name: string) => Promise<ThemeSelectResult>;
  pending: boolean;
} {
  const [pending, setPending] = useState(false);

  const select = useCallback(
    async (name: string): Promise<ThemeSelectResult> => {
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
    },
    [],
  );

  return { select, pending };
}

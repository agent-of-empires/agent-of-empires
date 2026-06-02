import { useCallback, useState } from "react";
import { fetchProfiles, updateProfileSettings } from "../lib/api";
import { dispatchThemePickerChanged } from "./useResolvedTheme";

export type ThemeSelectResult = { ok: true } | { ok: false; error: string };

const SAVE_ERROR = "Could not save theme. Please try again.";

/** The profile a first-run user has no reason to pick yet: the default one,
 *  matching how the settings profile picker resolves the active profile. */
async function resolveDefaultProfile(): Promise<string> {
  const profiles = await fetchProfiles();
  return profiles.find((p) => p.is_default)?.name ?? "default";
}

/**
 * Persist a theme selection to the default profile and repaint, used by the
 * first-run theme welcome modal. Persist-then-paint: the dashboard only
 * repaints after the PATCH lands (via dispatchThemePickerChanged, which routes
 * through useResolvedTheme's single apply path), so a failed save never leaves
 * the applied/cached theme ahead of what is on disk (the #1510 bug class).
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
        const profile = await resolveDefaultProfile();
        const ok = await updateProfileSettings(profile, { theme: { name } });
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

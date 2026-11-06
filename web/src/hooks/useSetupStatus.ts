import { useCallback, useEffect, useState } from "react";
import { fetchSetup, updateSetup, type SetupPatch, type SetupStatus } from "../lib/api";
import { onSettingsChanged } from "../lib/settingsEvents";
import { reportError } from "../lib/toastBus";

/**
 * Setup status for writable dashboards. `undefined` while loading; `null` when
 * setup doesn't apply (read-only, CityHall) or the read failed, which callers
 * treat as finished so nobody is trapped in the wizard.
 * `enabled` is `null` until `/api/about` answers.
 */
export function useSetupStatus(enabled: boolean | null) {
  const [status, setStatus] = useState<SetupStatus | null | undefined>(undefined);

  const refresh = useCallback(async () => {
    const next = await fetchSetup();
    setStatus(next);
    return next;
  }, []);

  useEffect(() => {
    if (!enabled) return;
    let live = true;
    const load = () =>
      void fetchSetup().then((next) => {
        if (live) setStatus(next);
      });
    load();
    // Steps derive from config, so a save made in Settings can change them.
    const unsubscribe = onSettingsChanged(load);
    return () => {
      live = false;
      unsubscribe();
    };
  }, [enabled]);

  const update = useCallback(
    async (patch: SetupPatch) => {
      const result = await updateSetup(patch);
      if (!result.ok) reportError(result.message ?? "Couldn't save setup progress.");
      await refresh();
    },
    [refresh],
  );

  return { status: enabled === false ? null : status, refresh, update };
}

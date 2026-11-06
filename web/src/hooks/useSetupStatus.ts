import { useCallback, useEffect, useState } from "react";
import { fetchSetup, updateSetup, type SetupPatch, type SetupStatus } from "../lib/api";

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
    void fetchSetup().then((next) => {
      if (live) setStatus(next);
    });
    return () => {
      live = false;
    };
  }, [enabled]);

  const update = useCallback(
    async (patch: SetupPatch) => {
      await updateSetup(patch);
      await refresh();
    },
    [refresh],
  );

  return { status: enabled === false ? null : status, refresh, update };
}

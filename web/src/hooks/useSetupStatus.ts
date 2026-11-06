import { useCallback, useEffect, useRef, useState } from "react";
import { fetchSetup, updateSetup, type SetupPatch, type SetupStatus } from "../lib/api";
import { onSettingsChanged } from "../lib/settingsEvents";
import { reportError } from "../lib/toastBus";

/**
 * Setup status for writable dashboards. `undefined` while loading; `null` when
 * setup doesn't apply (read-only, CityHall) or the read failed, which callers
 * treat as finished so nobody is trapped in the wizard.
 * `enabled` is `null` until `/api/about` answers. A first read that outlasts
 * the deadline counts as failed; a later failed read keeps the last status.
 */
export const SETUP_READ_DEADLINE_MS = 10_000;

export function useSetupStatus(enabled: boolean | null) {
  const [status, setStatus] = useState<SetupStatus | null | undefined>(undefined);

  // Every read takes a number; only the newest one may set status.
  const seq = useRef({ latest: 0 });

  const refresh = useCallback(async () => {
    const id = ++seq.current.latest;
    const next = await fetchSetup();
    if (id === seq.current.latest) setStatus((prev) => next ?? prev ?? null);
    return next;
  }, []);

  useEffect(() => {
    if (!enabled) return;
    const reads = seq.current;
    const deadline = setTimeout(() => {
      reads.latest++;
      setStatus((prev) => (prev === undefined ? null : prev));
    }, SETUP_READ_DEADLINE_MS);
    void refresh().finally(() => clearTimeout(deadline));
    // Steps derive from config, so a save made in Settings can change them.
    const unsubscribe = onSettingsChanged(() => void refresh());
    return () => {
      clearTimeout(deadline);
      reads.latest++;
      unsubscribe();
    };
  }, [enabled, refresh]);

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

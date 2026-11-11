import { useCallback, useEffect, useRef, useState } from "react";
import { elevateLogin, updateMachineSettingsInline } from "../lib/api";

export interface ElevationRequest {
  submit: (passphrase: string) => Promise<string | null>;
  cancel: () => void;
}

/** Machine-wide saves that ask for the passphrase inline, then retry the same write once. */
export function useInlineMachineSave() {
  const [elevation, setElevation] = useState<ElevationRequest | null>(null);
  const pending = useRef<ElevationRequest | null>(null);
  const latest = useRef(0);
  useEffect(() => () => pending.current?.cancel(), []);

  const save = useCallback(async (updates: Record<string, unknown>): Promise<boolean> => {
    const id = ++latest.current;
    const outcome = await updateMachineSettingsInline(updates);
    // `uncertain` isn't success: the caller can't know the value it shows was saved.
    if (outcome !== "elevation_required") return outcome === "committed";
    // A newer write started first owns the prompt; this late 403 must not replace it.
    if (id !== latest.current) return false;
    // A newer write cancels the pending one, so every save promise settles.
    pending.current?.cancel();
    return new Promise<boolean>((resolve) => {
      const finish = (ok: boolean) => {
        if (pending.current === request) {
          pending.current = null;
          setElevation(null);
        }
        resolve(ok);
      };
      const request: ElevationRequest = {
        cancel: () => finish(false),
        submit: async (passphrase) => {
          const result = await elevateLogin(passphrase);
          if (!result.ok) return result.error ?? "That passphrase did not match.";
          // Cancelled while elevating: its edit already reverted, so it must not write.
          if (pending.current !== request) return null;
          const retried = await updateMachineSettingsInline(updates);
          finish(retried === "committed");
          return null;
        },
      };
      pending.current = request;
      setElevation(request);
    });
  }, []);

  return { save, elevation };
}

import { useCallback, useState } from "react";
import { elevateLogin, updateMachineSettingsInline } from "../lib/api";

export interface ElevationRequest {
  submit: (passphrase: string) => Promise<string | null>;
  cancel: () => void;
}

/** Machine-wide saves that ask for the passphrase inline, then retry the same write once. */
export function useInlineMachineSave() {
  const [elevation, setElevation] = useState<ElevationRequest | null>(null);

  const save = useCallback(async (updates: Record<string, unknown>): Promise<boolean> => {
    const outcome = await updateMachineSettingsInline(updates);
    if (outcome !== "elevation_required") return outcome !== "rejected";
    return new Promise<boolean>((resolve) => {
      const finish = (ok: boolean) => {
        setElevation(null);
        resolve(ok);
      };
      setElevation({
        cancel: () => finish(false),
        submit: async (passphrase) => {
          const result = await elevateLogin(passphrase);
          if (!result.ok) return result.error ?? "That passphrase did not match.";
          const retried = await updateMachineSettingsInline(updates);
          finish(retried === "committed" || retried === "uncertain");
          return null;
        },
      });
    });
  }, []);

  return { save, elevation };
}

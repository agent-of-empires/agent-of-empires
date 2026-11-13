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
  const queue = useRef<Promise<unknown>>(Promise.resolve());
  useEffect(() => () => pending.current?.cancel(), []);

  const write = useCallback(async (id: number, updates: Record<string, unknown>): Promise<boolean> => {
    const outcome = await updateMachineSettingsInline(updates);
    // `uncertain` isn't success: the caller can't know the value it shows was saved.
    if (outcome !== "elevation_required") return outcome === "committed";
    // A newer save was issued while this one was out; it owns any prompt.
    if (id !== latest.current) return false;
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
          // Past the prompt: a newer save now queues behind this retry instead of racing it.
          pending.current = null;
          setElevation(null);
          resolve((await updateMachineSettingsInline(updates)) === "committed");
          return null;
        },
      };
      pending.current = request;
      setElevation(request);
    });
  }, []);

  // Writes run one at a time so acknowledgements resolve in issue order.
  const save = useCallback(
    (updates: Record<string, unknown>): Promise<boolean> => {
      const id = ++latest.current;
      // Settles a waiting prompt so it can't block this write or retry over it.
      pending.current?.cancel();
      const run = queue.current.then(() => write(id, updates));
      queue.current = run.catch(() => {});
      return run;
    },
    [write],
  );

  return { save, elevation };
}

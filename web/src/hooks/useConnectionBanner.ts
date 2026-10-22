import { useEffect, useState } from "react";
import { isServerDown, onServerDownChange } from "../lib/connectionState";

const RECONNECTED_FLASH_MS = 3000;

export type ConnectionBannerState = "down" | "reconnected";

/** `down` while the Daemon is unreachable, then `reconnected` for a brief flash once it returns. */
export function useConnectionBanner(): ConnectionBannerState | null {
  const [state, setState] = useState<ConnectionBannerState | null>(() => (isServerDown() ? "down" : null));

  useEffect(() => {
    let flash: ReturnType<typeof setTimeout> | undefined;
    const unsubscribe = onServerDownChange((down) => {
      clearTimeout(flash);
      if (down) {
        setState("down");
        return;
      }
      setState("reconnected");
      flash = setTimeout(() => setState(null), RECONNECTED_FLASH_MS);
    });
    return () => {
      unsubscribe();
      clearTimeout(flash);
    };
  }, []);

  return state;
}

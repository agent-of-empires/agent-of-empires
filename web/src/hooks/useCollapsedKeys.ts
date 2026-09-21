import { useCallback, useEffect, useState } from "react";
import { safeGetItem, safeRemoveItem, safeSetItem } from "../lib/safeStorage";

export interface CollapsedKeys {
  isCollapsed: (key: string) => boolean;
  toggle: (key: string) => void;
}

/** Collapsed flags for one sidebar level, remembered under `prefix` in local storage. */
export function useCollapsedKeys(prefix: string): CollapsedKeys {
  const [map, setMap] = useState<Record<string, boolean>>({});
  const stored = useCallback((key: string) => safeGetItem(`${prefix}${key}`) === "1", [prefix]);
  const isCollapsed = useCallback((key: string) => map[key] ?? stored(key), [map, stored]);
  // Keep the updater pure: StrictMode double-invokes it, so persist in an effect.
  const toggle = useCallback(
    (key: string) => setMap((prev) => ({ ...prev, [key]: !(prev[key] ?? stored(key)) })),
    [stored],
  );

  useEffect(() => {
    for (const [key, collapsed] of Object.entries(map)) {
      if (collapsed) safeSetItem(`${prefix}${key}`, "1");
      else safeRemoveItem(`${prefix}${key}`);
    }
  }, [map, prefix]);

  return { isCollapsed, toggle };
}

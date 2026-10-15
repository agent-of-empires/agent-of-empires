import { useCallback, useState } from "react";
import { safeGetItem, safeSetItem } from "../lib/safeStorage";

/** A boolean persisted as "true"/"false"; a missing or other value reads as `fallback`. */
export function usePersistedFlag(key: string, fallback: boolean) {
  const [value, setValue] = useState(() => {
    const raw = safeGetItem(key);
    return raw === "true" ? true : raw === "false" ? false : fallback;
  });
  const toggle = useCallback(() => {
    setValue((prev) => {
      safeSetItem(key, prev ? "false" : "true");
      return !prev;
    });
  }, [key]);
  return [value, toggle] as const;
}

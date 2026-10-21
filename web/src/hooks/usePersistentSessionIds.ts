import { useEffect, useState } from "react";

/**
 * Ids to keep mounted: the active session first, then the most recently
 * active ones, never more than `limit`. Without `persistent`, only the active one.
 */
export function usePersistentSessionIds(
  activeId: string,
  available: { has(id: string): boolean },
  persistent: boolean,
  limit: number,
): string[] {
  const [recentIds, setRecentIds] = useState<string[]>([]);

  useEffect(() => {
    let cancelled = false;
    queueMicrotask(() => {
      if (cancelled) return;
      if (!persistent) {
        setRecentIds([]);
        return;
      }
      setRecentIds((ids) => {
        const inactive = ids
          .filter((id) => id !== activeId)
          .filter((id) => available.has(id))
          .slice(0, Math.max(0, limit - 1));
        const next = [activeId, ...inactive];
        return next.join("\0") === ids.join("\0") ? ids : next;
      });
    });
    return () => {
      cancelled = true;
    };
  }, [activeId, available, limit, persistent]);

  // Sliced here so a stale list never mounts one view over the limit before the effect runs.
  return persistent ? [activeId, ...recentIds.filter((id) => id !== activeId)].slice(0, limit) : [activeId];
}

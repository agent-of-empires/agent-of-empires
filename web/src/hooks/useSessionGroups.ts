import { useCallback, useEffect, useMemo, useState } from "react";
import type { Workspace } from "../lib/types";
import { safeGetItem, safeRemoveItem, safeSetItem } from "../lib/safeStorage";
import { buildSessionGroups, type SidebarGroup } from "../lib/sidebarGroups";
import type { SidebarSortMode } from "../lib/sidebarSort";
import { useIdleDecayWindowMs } from "../lib/idleDecay";

// Distinct from the repo axis prefix (`aoe-repo-collapsed-`) so collapse
// state is per-axis: collapsing a user group never changes a repo group's
// state and vice versa. See #1234.
const COLLAPSED_KEY_PREFIX = "aoe-group-collapsed-";

function loadCollapsed(id: string): boolean {
  return safeGetItem(`${COLLAPSED_KEY_PREFIX}${id}`) === "1";
}

export function useSessionGroups(
  workspaces: Workspace[],
  sortMode: SidebarSortMode,
): {
  groups: SidebarGroup[];
  toggleGroupCollapsed: (groupId: string) => void;
} {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const [collapsedMap, setCollapsedMap] = useState<Record<string, boolean>>({});

  const groups = useMemo(
    () =>
      buildSessionGroups(workspaces, {
        idleDecayWindowMs,
        sortMode,
        isCollapsed: (id) => collapsedMap[id] ?? loadCollapsed(id),
      }),
    [workspaces, idleDecayWindowMs, sortMode, collapsedMap],
  );

  // The updater stays pure: it reads the current value but performs no
  // storage IO. Writing inside the updater is unsafe under React
  // StrictMode, which double-invokes updaters in dev: the first pass's
  // write would make the second pass's `loadCollapsed` read see the new
  // value and compute the opposite result, so the toggle no-ops and
  // storage desyncs from state. Persistence runs in an effect instead.
  const toggleGroupCollapsed = useCallback((groupId: string) => {
    setCollapsedMap((prev) => {
      const current = prev[groupId] ?? loadCollapsed(groupId);
      return { ...prev, [groupId]: !current };
    });
  }, []);

  useEffect(() => {
    for (const [id, collapsed] of Object.entries(collapsedMap)) {
      if (collapsed) {
        safeSetItem(`${COLLAPSED_KEY_PREFIX}${id}`, "1");
      } else {
        safeRemoveItem(`${COLLAPSED_KEY_PREFIX}${id}`);
      }
    }
  }, [collapsedMap]);

  return { groups, toggleGroupCollapsed };
}

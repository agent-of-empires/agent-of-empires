import { useCallback, useEffect, useMemo, useState } from "react";
import type { RepoGroup } from "../lib/types";
import { safeGetItem, safeRemoveItem, safeSetItem } from "../lib/safeStorage";
import { buildNestedSidebarGroups, type NestedSidebarGroup } from "../lib/sidebarGroups";
import type { PluginSortContext, SidebarSortMode } from "../lib/sidebarSort";
import { useIdleDecayWindowMs } from "../lib/idleDecay";

// Distinct from both the repo prefix (`aoe-repo-collapsed-`) and the flat
// group prefix (`aoe-group-collapsed-`): a subgroup only exists inside one
// repo, so its collapse state is keyed on the repo plus the group path and
// never shared with the flat group axis. See #1720.
const COLLAPSED_KEY_PREFIX = "aoe-nested-group-collapsed-";

// Encode both halves so a `::` inside a repo path or group path cannot make
// two different (repo, group) pairs collapse the same key. Keying on the
// raw `groupPath` ("" for Ungrouped) also sidesteps the `UNGROUPED_GROUP_ID`
// sentinel, so a literal user group named like the sentinel stays distinct.
function subgroupKey(repoId: string, groupPath: string): string {
  return `${encodeURIComponent(repoId)}::${encodeURIComponent(groupPath)}`;
}

function loadCollapsed(key: string): boolean {
  return safeGetItem(`${COLLAPSED_KEY_PREFIX}${key}`) === "1";
}

export function useNestedSidebarGroups(
  repoGroups: RepoGroup[],
  sortMode: SidebarSortMode,
  pluginSort?: PluginSortContext,
): {
  groups: NestedSidebarGroup[];
  toggleSubgroupCollapsed: (repoId: string, groupPath: string) => void;
} {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const [collapsedMap, setCollapsedMap] = useState<Record<string, boolean>>({});

  const groups = useMemo(
    () =>
      buildNestedSidebarGroups(repoGroups, {
        idleDecayWindowMs,
        sortMode,
        pluginSort,
        isSubgroupCollapsed: (repoId, groupPath) => {
          const key = subgroupKey(repoId, groupPath);
          return collapsedMap[key] ?? loadCollapsed(key);
        },
      }),
    [repoGroups, idleDecayWindowMs, sortMode, pluginSort, collapsedMap],
  );

  // The updater stays pure and persistence runs in an effect, for the same
  // StrictMode double-invoke reason documented in `useSessionGroups`.
  const toggleSubgroupCollapsed = useCallback((repoId: string, groupPath: string) => {
    const key = subgroupKey(repoId, groupPath);
    setCollapsedMap((prev) => {
      const current = prev[key] ?? loadCollapsed(key);
      return { ...prev, [key]: !current };
    });
  }, []);

  useEffect(() => {
    for (const [key, collapsed] of Object.entries(collapsedMap)) {
      if (collapsed) {
        safeSetItem(`${COLLAPSED_KEY_PREFIX}${key}`, "1");
      } else {
        safeRemoveItem(`${COLLAPSED_KEY_PREFIX}${key}`);
      }
    }
  }, [collapsedMap]);

  return { groups, toggleSubgroupCollapsed };
}

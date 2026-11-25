import { useCallback, useMemo } from "react";
import type { RepoGroup, Workspace } from "../lib/types";
import { buildNestedSidebarGroups, type NestedSidebarGroup } from "../lib/sidebarGroups";
import { useIdleDecayWindowMs } from "../lib/idleDecay";
import { useCollapsedKeys } from "./useCollapsedKeys";

// Encode both halves so a `::` in a path cannot collide two pairs.
const subgroupKey = (repoId: string, groupPath: string) =>
  `${encodeURIComponent(repoId)}::${encodeURIComponent(groupPath)}`;

export function useNestedSidebarGroups(
  repoGroups: RepoGroup[],
  compareWorkspace: (a: Workspace, b: Workspace) => number,
  comparePaths?: (a: string, b: string) => number,
): {
  groups: NestedSidebarGroup[];
  toggleSubgroupCollapsed: (repoId: string, groupPath: string) => void;
} {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const { isCollapsed, toggle } = useCollapsedKeys("aoe-nested-group-collapsed-");

  const groups = useMemo(
    () =>
      buildNestedSidebarGroups(repoGroups, {
        idleDecayWindowMs,
        compareWorkspace,
        comparePaths,
        isSubgroupCollapsed: (repoId, groupPath) => isCollapsed(subgroupKey(repoId, groupPath)),
      }),
    [repoGroups, idleDecayWindowMs, compareWorkspace, comparePaths, isCollapsed],
  );

  const toggleSubgroupCollapsed = useCallback(
    (repoId: string, groupPath: string) => toggle(subgroupKey(repoId, groupPath)),
    [toggle],
  );

  return { groups, toggleSubgroupCollapsed };
}

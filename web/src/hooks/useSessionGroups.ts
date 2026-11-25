import { useMemo } from "react";
import type { Workspace } from "../lib/types";
import { buildSessionGroups, type SidebarGroup } from "../lib/sidebarGroups";
import { useIdleDecayWindowMs } from "../lib/idleDecay";
import { useCollapsedKeys } from "./useCollapsedKeys";

export function useSessionGroups(
  workspaces: Workspace[],
  compareWorkspace: (a: Workspace, b: Workspace) => number,
  comparePaths?: (a: string, b: string) => number,
): {
  groups: SidebarGroup[];
  toggleGroupCollapsed: (groupId: string) => void;
} {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const { isCollapsed, toggle } = useCollapsedKeys("aoe-group-collapsed-");

  const groups = useMemo(
    () => buildSessionGroups(workspaces, { idleDecayWindowMs, compareWorkspace, comparePaths, isCollapsed }),
    [workspaces, idleDecayWindowMs, compareWorkspace, comparePaths, isCollapsed],
  );

  return { groups, toggleGroupCollapsed: toggle };
}

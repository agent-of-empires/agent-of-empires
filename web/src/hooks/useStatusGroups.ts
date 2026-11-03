import { useMemo } from "react";
import type { Workspace } from "../lib/types";
import { buildStatusGroups, type SidebarGroup } from "../lib/sidebarGroups";
import type { PluginSortContext, SidebarSortMode } from "../lib/sidebarSort";
import { useIdleDecayWindowMs } from "../lib/idleDecay";
import { useCollapsedKeys } from "./useCollapsedKeys";

export function useStatusGroups(
  workspaces: Workspace[],
  sortMode: SidebarSortMode,
  pluginSort?: PluginSortContext,
  pluginAttention?: ReadonlySet<string>,
): {
  groups: SidebarGroup[];
  toggleGroupCollapsed: (groupId: string) => void;
} {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const { isCollapsed, toggle } = useCollapsedKeys("aoe-status-collapsed-");

  const groups = useMemo(
    () => buildStatusGroups(workspaces, { idleDecayWindowMs, sortMode, pluginSort, pluginAttention, isCollapsed }),
    [workspaces, idleDecayWindowMs, sortMode, pluginSort, pluginAttention, isCollapsed],
  );

  return { groups, toggleGroupCollapsed: toggle };
}

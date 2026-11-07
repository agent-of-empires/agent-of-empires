import { useMemo } from "react";
import type { Workspace } from "../lib/types";
import { buildStatusGroups, type SidebarGroup } from "../lib/sidebarGroups";
import type { PluginSortContext, SidebarSortMode } from "../lib/sidebarSort";
import { useIdleDecayWindowMs } from "../lib/idleDecay";
import { useCollapsedKeys } from "./useCollapsedKeys";

/** Collapse keys for status buckets, registered as a synced prefix in `webUiSync`. */
export const STATUS_COLLAPSE_PREFIX = "aoe-status-collapsed-";

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
  const { isCollapsed, toggle } = useCollapsedKeys(STATUS_COLLAPSE_PREFIX);

  const groups = useMemo(
    () => buildStatusGroups(workspaces, { idleDecayWindowMs, sortMode, pluginSort, pluginAttention, isCollapsed }),
    [workspaces, idleDecayWindowMs, sortMode, pluginSort, pluginAttention, isCollapsed],
  );

  return { groups, toggleGroupCollapsed: toggle };
}

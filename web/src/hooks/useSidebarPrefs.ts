import { useCallback, useState } from "react";
import { loadSidebarAxis, saveSidebarAxis, type SidebarAxis } from "../lib/sidebarAxis";
import { loadSidebarSortMode, saveSidebarSortMode, type SidebarSortMode } from "../lib/sidebarSort";
import { reportFeatureUse, type FeatureUse } from "../lib/featureUse";

const SORT_FEATURE: Record<SidebarSortMode, FeatureUse> = {
  manual: "sort_manual",
  lastActivity: "sort_last_activity",
  attention: "sort_attention",
};

const AXIS_FEATURE: Record<SidebarAxis, FeatureUse> = {
  repo: "axis_repo",
  org: "axis_org",
  group: "axis_group",
  "repo+group": "axis_repo_group",
  status: "axis_status",
};

function saveAxis(axis: SidebarAxis): void {
  saveSidebarAxis(axis);
  reportFeatureUse(AXIS_FEATURE[axis]);
}

function saveSortMode(mode: SidebarSortMode): void {
  saveSidebarSortMode(mode);
  reportFeatureUse(SORT_FEATURE[mode]);
}

function usePersistedChoice<T>(load: () => T, save: (next: T) => void): readonly [T, (next: T) => void] {
  const [value, setValue] = useState<T>(load);
  const update = useCallback(
    (next: T) => {
      setValue(next);
      save(next);
    },
    [save],
  );
  return [value, update] as const;
}

export function useSidebarAxis(): readonly [SidebarAxis, (axis: SidebarAxis) => void] {
  return usePersistedChoice(loadSidebarAxis, saveAxis);
}

export function useSidebarSortMode(): readonly [SidebarSortMode, (mode: SidebarSortMode) => void] {
  return usePersistedChoice(loadSidebarSortMode, saveSortMode);
}

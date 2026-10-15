import { useCallback, useState } from "react";
import { loadSidebarAxis, saveSidebarAxis, type SidebarAxis } from "../lib/sidebarAxis";
import { loadSidebarSortMode, saveSidebarSortMode, type SidebarSortMode } from "../lib/sidebarSort";

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
  return usePersistedChoice(loadSidebarAxis, saveSidebarAxis);
}

export function useSidebarSortMode(): readonly [SidebarSortMode, (mode: SidebarSortMode) => void] {
  return usePersistedChoice(loadSidebarSortMode, saveSidebarSortMode);
}

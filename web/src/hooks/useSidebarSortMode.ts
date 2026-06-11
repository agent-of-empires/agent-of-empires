import { useCallback, useState } from "react";
import { loadSidebarSortMode, saveSidebarSortMode, type SidebarSortMode } from "../lib/sidebarSort";

export function useSidebarSortMode(): readonly [SidebarSortMode, (mode: SidebarSortMode) => void] {
  const [mode, setMode] = useState<SidebarSortMode>(loadSidebarSortMode);

  const update = useCallback((next: SidebarSortMode) => {
    setMode(next);
    saveSidebarSortMode(next);
  }, []);

  return [mode, update] as const;
}

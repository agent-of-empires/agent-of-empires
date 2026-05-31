import { useCallback, useState } from "react";
import {
  loadSidebarAxis,
  saveSidebarAxis,
  type SidebarAxis,
} from "../lib/sidebarAxis";

export function useSidebarAxis(): readonly [
  SidebarAxis,
  (axis: SidebarAxis) => void,
] {
  const [axis, setAxis] = useState<SidebarAxis>(loadSidebarAxis);

  const update = useCallback((next: SidebarAxis) => {
    setAxis(next);
    saveSidebarAxis(next);
  }, []);

  return [axis, update] as const;
}

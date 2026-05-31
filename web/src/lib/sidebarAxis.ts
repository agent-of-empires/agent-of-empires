import { safeGetItem, safeSetItem } from "./safeStorage";

/** Which organisation axis the sidebar groups sessions by: the auto-derived
 *  repository axis (default, the original behavior) or the user-defined
 *  group axis backed by each session's `group_path`. Per-browser, like the
 *  sort mode. See #1234. */
export type SidebarAxis = "repo" | "group";

export const SIDEBAR_AXIS_KEY = "aoe-sidebar-axis";

export function loadSidebarAxis(): SidebarAxis {
  return safeGetItem(SIDEBAR_AXIS_KEY) === "group" ? "group" : "repo";
}

export function saveSidebarAxis(axis: SidebarAxis): void {
  safeSetItem(SIDEBAR_AXIS_KEY, axis);
}

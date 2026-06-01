import { safeGetItem, safeSetItem } from "./safeStorage";

/** Which organisation axis the sidebar groups sessions by: the auto-derived
 *  repository axis (default, the original behavior), the user-defined group
 *  axis backed by each session's `group_path`, or the nested `repo+group`
 *  axis that keeps repository headers and nests user groups inside each one.
 *  Per-browser, like the sort mode. See #1234, #1720. */
export type SidebarAxis = "repo" | "group" | "repo+group";

export const SIDEBAR_AXIS_KEY = "aoe-sidebar-axis";

export function loadSidebarAxis(): SidebarAxis {
  const value = safeGetItem(SIDEBAR_AXIS_KEY);
  return value === "group" || value === "repo+group" ? value : "repo";
}

export function saveSidebarAxis(axis: SidebarAxis): void {
  safeSetItem(SIDEBAR_AXIS_KEY, axis);
}

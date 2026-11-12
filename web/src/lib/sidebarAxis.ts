import { safeGetItem, safeSetItem } from "./safeStorage";

/** Which organisation axis the sidebar groups sessions by: the auto-derived
 *  repository axis (default, the original behavior), the org axis derived
 *  from each repo's remote owner (any hosted git remote, not just GitHub),
 *  the user-defined group axis backed by each session's `group_path`, or
 *  the nested `repo+group` axis that keeps repository headers and nests
 *  user groups inside each one. Per-browser, like the sort mode. See
 *  #1234, #1720, #3283. */
export type SidebarAxis = "repo" | "org" | "group" | "repo+group";

// Keyed by axis, so a new axis must say whether its headers are repositories.
const DRAWS_REPO_HEADERS: Record<SidebarAxis, boolean> = {
  repo: true,
  org: true,
  "repo+group": true,
  group: false,
};

/** Whether the axis draws repository headers, which is where a Project with live Sessions, or a pinned one,
 *  appears. Without them, the Projects section lists every registered Project (#5065). */
export function axisDrawsRepoHeaders(axis: SidebarAxis): boolean {
  return DRAWS_REPO_HEADERS[axis];
}

export const SIDEBAR_AXIS_KEY = "aoe-sidebar-axis";

export function loadSidebarAxis(): SidebarAxis {
  const value = safeGetItem(SIDEBAR_AXIS_KEY);
  return value === "org" || value === "group" || value === "repo+group" ? value : "repo";
}

export function saveSidebarAxis(axis: SidebarAxis): void {
  safeSetItem(SIDEBAR_AXIS_KEY, axis);
}

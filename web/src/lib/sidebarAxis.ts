import { safeGetItem, safeSetItem } from "./safeStorage";

/** Sidebar grouping axis, persisted per browser. */
export type SidebarAxis = "repo" | "org" | "group" | "repo+group" | "status";

// Keyed by axis, so a new axis must say whether its headers are repositories.
const DRAWS_REPO_HEADERS: Record<SidebarAxis, boolean> = {
  repo: true,
  org: true,
  "repo+group": true,
  group: false,
  status: false,
};

/** Registered projects appear separately when the axis has no repository headers. */
export function axisDrawsRepoHeaders(axis: SidebarAxis): boolean {
  return DRAWS_REPO_HEADERS[axis];
}

export const SIDEBAR_AXIS_KEY = "aoe-sidebar-axis";

export function loadSidebarAxis(): SidebarAxis {
  const value = safeGetItem(SIDEBAR_AXIS_KEY);
  return value === "org" || value === "group" || value === "repo+group" || value === "status" ? value : "repo";
}

export function saveSidebarAxis(axis: SidebarAxis): void {
  safeSetItem(SIDEBAR_AXIS_KEY, axis);
}

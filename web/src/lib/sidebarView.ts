import type { RuntimeCursor, SidebarView } from "./api";
import type { FeatureUse } from "./featureUse";

export type SidebarGroupBy = SidebarView["group_by"];
export type SidebarSort = SidebarView["sort"];

/** What a Daemon that predates the shared view shows. */
export const DEFAULT_SIDEBAR_VIEW: SidebarView = { group_by: "project", sort: "custom" };

export const GROUP_BY_LABEL: Record<SidebarGroupBy, string> = { project: "Repository", group: "My groups" };
export const SORT_LABEL: Record<SidebarSort, string> = { custom: "Custom", name: "Name" };

export const GROUP_BY_FEATURE: Record<SidebarGroupBy, FeatureUse> = {
  project: "group_by_project",
  group: "group_by_manual",
};
export const SORT_FEATURE: Record<SidebarSort, FeatureUse> = { custom: "sort_custom", name: "sort_az" };

/**
 * Local writes of one Daemon value that a poll must not undo. A poll is held off while any write is in
 * flight, and after that until it reads a snapshot at least as new as the newest receipt. A failed write
 * leaves no receipt, so the next poll restores what the Daemon holds.
 */
export interface WriteHold {
  pending: number;
  receipt: RuntimeCursor | null;
}

export const newWriteHold = (): WriteHold => ({ pending: 0, receipt: null });

export function holdStarted(hold: WriteHold): void {
  hold.pending += 1;
}

export function holdSettled(hold: WriteHold, receipt: RuntimeCursor | null): void {
  hold.pending = Math.max(0, hold.pending - 1);
  if (!receipt) return;
  const held = hold.receipt;
  if (!held || held.epoch !== receipt.epoch || held.revision < receipt.revision) hold.receipt = receipt;
}

/** Whether a poll read at `cursor` may replace the locally written value. A new epoch is a restarted Daemon. */
export function pollMayReplace(hold: WriteHold, cursor: RuntimeCursor): boolean {
  if (hold.pending > 0) return false;
  const receipt = hold.receipt;
  return !receipt || receipt.epoch !== cursor.epoch || cursor.revision >= receipt.revision;
}

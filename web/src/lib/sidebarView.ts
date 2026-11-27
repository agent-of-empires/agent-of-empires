import type { RuntimeCursor, SidebarView, SidebarViewPatch } from "./api";
import type { FeatureUse } from "./featureUse";

export type SidebarGroupBy = SidebarView["group_by"];
export type SidebarSort = SidebarView["sort"];

/** What a Daemon that predates the shared view shows. */
export const DEFAULT_SIDEBAR_VIEW: SidebarView = { group_by: "project", sort: "custom" };

export const GROUP_BY_LABEL: Record<SidebarGroupBy, string> = { project: "Repository", group: "Groups" };
export const SORT_LABEL: Record<SidebarSort, string> = { custom: "Custom", newest: "Newest first", name: "Name" };

/** The view as this browser's own patch leaves it: a grouping or sort pick is the web's, and a sort pick also
 *  clears the plugin sort marker, as the Daemon does. */
export function applyViewPatch(view: SidebarView, patch: SidebarViewPatch): SidebarView {
  const next: SidebarView = { ...view };
  if (patch.group_by) next.group_by = patch.group_by;
  if (patch.sort) next.sort = patch.sort;
  if (patch.group_by || patch.sort) next.changed_by = "web";
  if (patch.sort || patch.web_plugin_sort === null) delete next.web_plugin_sort;
  else if (patch.web_plugin_sort) next.web_plugin_sort = patch.web_plugin_sort;
  return next;
}

export const sameView = (a: SidebarView, b: SidebarView) =>
  a.group_by === b.group_by &&
  a.sort === b.sort &&
  a.changed_by === b.changed_by &&
  a.web_plugin_sort === b.web_plugin_sort;

const PEER_PLACE = { tui: "in the TUI", cli: "in the CLI" } as const;

/** Why the list moved when another Client kind changed the view from `before` to `after`, or null when this
 *  dashboard's own kind did, or neither the grouping nor the sort changed. */
export function peerViewChangeNotice(before: SidebarView, after: SidebarView): string | null {
  const by = after.changed_by;
  if (!by || by === "web") return null;
  const grouping = before.group_by !== after.group_by;
  const sort = before.sort !== after.sort;
  if (!grouping && !sort) return null;
  const what = grouping && sort ? "Grouping and sort" : grouping ? "Grouping" : "Sort";
  return `${what} changed ${PEER_PLACE[by]}`;
}

export const GROUP_BY_FEATURE: Record<SidebarGroupBy, FeatureUse> = {
  project: "group_by_project",
  group: "group_by_manual",
};
export const SORT_FEATURE: Record<SidebarSort, FeatureUse> = {
  custom: "sort_custom",
  newest: "sort_newest",
  name: "sort_az",
};

/**
 * Local writes of one Daemon value that a poll must not undo. A poll is held off while any write is in
 * flight, and after that until it reads a snapshot at least as new as the newest receipt. A failed write,
 * including one past its deadline, leaves no receipt, so the next poll restores what the Daemon holds.
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

/** Runs writes of one Daemon value one after another in call order, so an older write never lands after a newer
 *  one. Each write keeps its own deadline, which starts when it is sent. */
export function newWriteQueue() {
  let tail: Promise<unknown> = Promise.resolve();
  return <T>(write: () => Promise<T>): Promise<T> => {
    const next = tail.then(write, write);
    tail = next.catch(() => {});
    return next;
  };
}

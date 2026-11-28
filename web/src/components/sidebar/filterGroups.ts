import type { Workspace } from "../../lib/types";
import {
  sidebarGroupHasLiveWorkspace,
  UNGROUPED_GROUP_ID,
  type NestedSidebarGroup,
  type SidebarGroup,
  type SidebarWorkspaceView,
} from "../../lib/sidebarGroups";
import { workspaceIsSunk, workspaceIsTrashed } from "../../lib/sidebarSort";

function workspaceMatchesQuery(ws: Workspace, q: string): boolean {
  return (
    ws.displayName.toLowerCase().includes(q) ||
    ws.projectPath.toLowerCase().includes(q) ||
    (ws.branch?.toLowerCase().includes(q) ?? false) ||
    ws.agents.some((a) => a.toLowerCase().includes(q)) ||
    ws.sessions.some((s) => s.title.toLowerCase().includes(q))
  );
}

/** A row survives when it or any enclosing header name matches `q`, and it passes the facet and Needs you
 *  filters. */
export function makeRowFilter(
  q: string,
  matchesFacets: (ws: Workspace) => boolean,
  needsYou?: (ws: Workspace) => boolean,
) {
  return (v: SidebarWorkspaceView, ...groupNames: string[]) =>
    (!q || workspaceMatchesQuery(v.workspace, q) || groupNames.some((n) => n.toLowerCase().includes(q))) &&
    matchesFacets(v.workspace) &&
    (!needsYou || needsYou(v.workspace));
}

type RowFilter = ReturnType<typeof makeRowFilter>;

/** Filters each group's rows and drops groups left empty. Under a repository the ungrouped rows have no header
 *  of their own, so no "Ungrouped" name to match. */
function filterLevel<G extends SidebarGroup>(groups: G[], keep: RowFilter, ...parentNames: string[]): G[] {
  return groups
    .map((g) => {
      const names =
        parentNames.length > 0 && g.id === UNGROUPED_GROUP_ID ? parentNames : [g.displayName, ...parentNames];
      return { ...g, workspaces: g.workspaces.filter((v) => keep(v, ...names)) };
    })
    .filter((g) => g.workspaces.length > 0);
}

export function filterFlat(groups: SidebarGroup[], keep: RowFilter): SidebarGroup[] {
  return filterLevel(groups, keep);
}

export function filterNested(groups: NestedSidebarGroup[], keep: RowFilter): NestedSidebarGroup[] {
  return groups
    .map((ng) => ({ repo: ng.repo, subgroups: filterLevel(ng.subgroups, keep, ng.repo.displayName) }))
    .filter((ng) => ng.subgroups.length > 0);
}

/** The subgroups the Repository grouping renders: a collapsed repo hides every subgroup under it, and its
 *  ungrouped rows have no header to collapse them. */
export function visibleSubgroups(groups: NestedSidebarGroup[], expandAll: boolean): SidebarGroup[] {
  return groups.flatMap((ng) =>
    expandAll || !ng.repo.collapsed
      ? ng.subgroups.map((sg) => (sg.id === UNGROUPED_GROUP_ID && sg.collapsed ? { ...sg, collapsed: false } : sg))
      : [],
  );
}

/** Footer order: archived rows newest archived first, then snoozed rows soonest wake first, then the rest newest
 *  accessed first. A snooze time is when it wakes, so it never competes with an archive time. */
function sinkRank(ws: Workspace): [bucket: number, key: number] {
  const times = (pick: (s: Workspace["sessions"][number]) => string | null | undefined) =>
    ws.sessions.flatMap((s) => {
      const ms = Date.parse(pick(s) ?? "");
      return Number.isFinite(ms) ? [ms] : [];
    });
  // An empty list gives -Infinity from max and Infinity from min, so an unparsable time sorts last.
  if (ws.sessions.some((s) => s.archived_at != null)) return [0, -Math.max(...times((s) => s.archived_at))];
  if (ws.sessions.some((s) => s.snoozed_until != null)) return [1, Math.min(...times((s) => s.snoozed_until))];
  return [2, -Math.max(...times((s) => s.last_accessed_at))];
}

function compareSunk(a: Workspace, b: Workspace): number {
  const [ab, ak] = sinkRank(a);
  const [bb, bk] = sinkRank(b);
  return ab - bb || (ak === bk ? 0 : ak < bk ? -1 : 1);
}

// Deduped by workspace: Groups slices one workspace into a Group per session group, so the
// footer lists it once with every archived session from those slices. First slice wins for
// the row identity. Rows follow `compareSunk` across groups, whatever the sidebar sort.
export const sunkViews = (groups: SidebarGroup[]) => {
  const byId = new Map<string, SidebarWorkspaceView>();
  for (const g of groups) {
    for (const v of g.workspaces) {
      if (!workspaceIsSunk(v.workspace) || workspaceIsTrashed(v.workspace)) continue;
      const kept = byId.get(v.workspace.id);
      if (!kept) {
        byId.set(v.workspace.id, v);
        continue;
      }
      const alreadyShown = new Set(kept.workspace.sessions.map((s) => s.id));
      const extra = v.workspace.sessions.filter((s) => !alreadyShown.has(s.id));
      if (extra.length > 0) {
        byId.set(v.workspace.id, {
          ...kept,
          workspace: { ...kept.workspace, sessions: [...kept.workspace.sessions, ...extra] },
        });
      }
    }
  }
  return [...byId.values()].sort((a, b) => compareSunk(a.workspace, b.workspace));
};

/** Rendered row keys in render order, so Shift+click ranges only span visible rows. `allGroups`
 *  supplies the footer, which ignores group collapse and lists each workspace once. */
export function renderedOrder(
  groups: SidebarGroup[],
  expandAll: boolean,
  sunkExpanded: boolean,
  allGroups: SidebarGroup[] = groups,
): string[] {
  const keys = new Set<string>();
  for (const g of groups) {
    if (!sidebarGroupHasLiveWorkspace(g) || !(expandAll || !g.collapsed)) continue;
    for (const v of g.workspaces) if (!workspaceIsSunk(v.workspace)) keys.add(v.key);
  }
  if (sunkExpanded) for (const v of sunkViews(allGroups)) keys.add(v.key);
  return [...keys];
}

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

/** The subgroups the Repository grouping renders: a collapsed repo hides every subgroup under it. */
export function visibleSubgroups(groups: NestedSidebarGroup[], expandAll: boolean): SidebarGroup[] {
  return groups.flatMap((ng) => (expandAll || !ng.repo.collapsed ? ng.subgroups : []));
}

function sinkTimeMs(ws: Workspace): number {
  let best = Number.NEGATIVE_INFINITY;
  for (const s of ws.sessions) {
    const ts = s.archived_at ?? s.snoozed_until ?? s.last_accessed_at;
    const ms = ts ? Date.parse(ts) : Number.NaN;
    if (ms > best) best = ms;
  }
  return best;
}

// Deduped by workspace: My groups slices one workspace into a Group per session group, so the
// footer lists it once with every archived session from those slices. First slice wins for
// the row identity. Rows sort newest sunk first across groups, whatever the sidebar sort, matching
// the TUI's Archived section.
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
  return [...byId.values()].sort((a, b) => sinkTimeMs(b.workspace) - sinkTimeMs(a.workspace) || 0);
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

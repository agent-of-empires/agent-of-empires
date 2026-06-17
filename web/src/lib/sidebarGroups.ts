import type { RepoColor } from "./repoAppearance";
import type { ProjectInfo, RepoGroup, SessionResponse, Workspace, WorkspaceStatus } from "./types";
import { isSessionActive } from "./session";
import { compareWorkspacesForComputedSortMode, type SidebarSortMode, workspaceIsSunk } from "./sidebarSort";
import { MULTI_REPO_GROUP_ID, SCRATCH_GROUP_ID } from "../hooks/useRepoGroups";

// Synthetic id for the bucket that collects sessions with no user-assigned
// `group_path`. Distinct from any real group path (a real path is never
// empty after trimming), so it can double as a localStorage collapse key.
export const UNGROUPED_GROUP_ID = "__ungrouped__";

// Which affordances a sidebar group header may show. The repo axis groups
// own appearance (alias/color), manual drag-reorder, and create-in-repo;
// the user-group axis owns none of these in v1, so they are gated here
// instead of by scattered `kind === ...` checks in the render path.
export interface SidebarGroupCapabilities {
  appearance: boolean;
  reorder: boolean;
  create: "repo" | "generic";
}

// A single rendered workspace row inside a sidebar group. `workspace`
// keeps its real server id for selection, routing, and delete actions; in
// the group axis its `sessions` is a per-group slice (a workspace whose
// sessions span groups appears once per group). `key` is a render/DnD
// identity that stays unique across such a split, so it must never be used
// as the workspace id for an action.
export interface SidebarWorkspaceView {
  key: string;
  workspace: Workspace;
}

// The honest render model for the sidebar. Repo groups map into it via
// `repoGroupToSidebarGroup`; user groups are built by `buildSessionGroups`.
// `RepoGroup` stays a repo-axis-internal type and is never reused to mean
// a user group.
export interface SidebarGroup {
  id: string;
  kind: "repo" | "sessionGroup";
  displayName: string;
  defaultDisplayName: string;
  alias: string | null;
  color: RepoColor | null;
  remoteOwner: string | null;
  workspaces: SidebarWorkspaceView[];
  status: WorkspaceStatus;
  collapsed: boolean;
  capabilities: SidebarGroupCapabilities;
  /** Set when `kind === "repo"`. */
  repoPath?: string;
  /** Set when `kind === "sessionGroup"`. Empty string for Ungrouped. */
  groupPath?: string;
  /** Registry entries (saved projects) for this repo path; empty when the
   *  repo is not saved. Present regardless of pin state, so the context menu
   *  can offer Pin/Unpin. Repo axis only. See #2047, #2208. */
  registeredProjects: ProjectInfo[];
  /** Derived: a saved entry for this repo has `pinned === true`. */
  pinned: boolean;
  /** Derived: pinned with no live workspace, so it shows as an empty header
   *  that only the pin keeps visible. */
  pinnedEmpty: boolean;
}

function isSyntheticRepoGroup(id: string): boolean {
  return id === MULTI_REPO_GROUP_ID || id === SCRATCH_GROUP_ID;
}

// Adapt a repo-axis `RepoGroup` into the shared render model without
// changing any repo behavior. Synthetic Multi-repo / Scratch buckets keep
// their generic create action (they route the `+` to the wizard, not to a
// repo path); real repos create directly in their repo.
export function repoGroupToSidebarGroup(group: RepoGroup): SidebarGroup {
  const synthetic = isSyntheticRepoGroup(group.id);
  // Pinned is the per-project flag, not mere registry membership: a
  // saved-but-unpinned project attaches its entry for the context menu but
  // shows no marker and no sessionless header. See #2208.
  const pinned = !synthetic && group.registeredProjects.some((p) => p.pinned);
  return {
    id: group.id,
    kind: "repo",
    displayName: group.displayName,
    defaultDisplayName: group.defaultDisplayName,
    alias: group.alias,
    color: group.color,
    remoteOwner: group.remoteOwner,
    workspaces: group.workspaces.map((workspace) => ({
      key: workspace.id,
      workspace,
    })),
    status: group.status,
    collapsed: group.collapsed,
    capabilities: {
      appearance: true,
      reorder: true,
      create: synthetic ? "generic" : "repo",
    },
    repoPath: group.repoPath,
    registeredProjects: synthetic ? [] : group.registeredProjects,
    pinned,
    pinnedEmpty: pinned && group.workspaces.length === 0,
  };
}

function normalizeGroupPath(path: string | null | undefined): string {
  const trimmed = (path ?? "").trim();
  if (trimmed === "") return "";
  // Strip leading/trailing slashes so "feature" and "feature/" bucket as
  // the same group instead of two perceived-identical entries.
  return trimmed.replace(/^\/+|\/+$/g, "");
}

function groupDisplayName(path: string): string {
  if (path === "") return "Ungrouped";
  // v1 renders groups flat; the leaf segment is the friendly label while
  // the full path stays available as the header title for nested groups.
  return path.split("/").pop() || path;
}

// Build the user-group axis from workspaces. `group_path` is per-session,
// so a workspace whose sessions span groups is split into one view per
// group, each carrying only that group's sessions. Sessions with an empty
// `group_path` collect into the Ungrouped bucket. Within a group, rows
// sort by the selected sort mode (the group axis has no manual order in
// v1, so `manual` falls back to last-activity); named groups sort
// alphabetically with Ungrouped pinned to the bottom regardless of mode.
export function buildSessionGroups(
  workspaces: Workspace[],
  opts: {
    idleDecayWindowMs: number;
    // Drives the within-group row comparator. `manual` falls back to
    // last-activity here (this axis has no manual drag order), while
    // `lastActivity` and `attention` are honored. See #1640.
    sortMode: SidebarSortMode;
    // `groupPath` is the normalized path ("" for Ungrouped), passed
    // alongside the synthetic id so nested callers can key collapse state
    // on the path and dodge the `UNGROUPED_GROUP_ID` sentinel. Flat callers
    // ignore it. See #1720.
    isCollapsed: (groupId: string, groupPath: string) => boolean;
  },
): SidebarGroup[] {
  const compareWorkspace = compareWorkspacesForComputedSortMode(opts.sortMode);
  const byGroup = new Map<string, SidebarWorkspaceView[]>();
  const order: string[] = [];

  for (const ws of workspaces) {
    const sessionsByGroup = new Map<string, SessionResponse[]>();
    for (const session of ws.sessions) {
      const gp = normalizeGroupPath(session.group_path);
      const existing = sessionsByGroup.get(gp);
      if (existing) existing.push(session);
      else sessionsByGroup.set(gp, [session]);
    }

    for (const [gp, sessions] of sessionsByGroup) {
      const sliced: Workspace = {
        ...ws,
        sessions,
        status: sessions.some((s) => isSessionActive(s, opts.idleDecayWindowMs)) ? "active" : "idle",
      };
      const view: SidebarWorkspaceView = {
        key: `${gp}::${ws.id}`,
        workspace: sliced,
      };
      const bucket = byGroup.get(gp);
      if (bucket) {
        bucket.push(view);
      } else {
        byGroup.set(gp, [view]);
        order.push(gp);
      }
    }
  }

  const groups: SidebarGroup[] = [];
  for (const gp of order) {
    const views = byGroup.get(gp)!;
    views.sort((a, b) => compareWorkspace(a.workspace, b.workspace));
    const id = gp === "" ? UNGROUPED_GROUP_ID : gp;
    const hasActive = views.some((v) => v.workspace.status === "active");
    groups.push({
      id,
      kind: "sessionGroup",
      displayName: groupDisplayName(gp),
      defaultDisplayName: groupDisplayName(gp),
      alias: null,
      color: null,
      remoteOwner: null,
      workspaces: views,
      status: hasActive ? "active" : "idle",
      collapsed: opts.isCollapsed(id, gp),
      capabilities: { appearance: false, reorder: false, create: "generic" },
      groupPath: gp,
      registeredProjects: [],
      pinned: false,
      pinnedEmpty: false,
    });
  }

  groups.sort((a, b) => {
    if (a.id === UNGROUPED_GROUP_ID) return 1;
    if (b.id === UNGROUPED_GROUP_ID) return -1;
    return a.displayName.localeCompare(b.displayName);
  });

  return groups;
}

// Group-axis equivalent of `repoGroupHasLiveWorkspace`: true while a group
// still has a row that has not dropped into the global "Snoozed & archived"
// footer, so an all-sunk group's header is not rendered empty.
export function sidebarGroupHasLiveWorkspace(group: SidebarGroup): boolean {
  return group.workspaces.some((v) => !workspaceIsSunk(v.workspace));
}

// Whether a group's header should render at all. A pinned-but-empty project
// has no live rows but must still show its header (that is the whole point
// of pinning), so it renders even though `sidebarGroupHasLiveWorkspace` is
// false. See #2047.
export function sidebarGroupShouldRender(group: SidebarGroup): boolean {
  return group.pinnedEmpty || sidebarGroupHasLiveWorkspace(group);
}

// The workspaces an "archive all in group" action would act on: every member
// whose primary session is not already archived. Triage targets each
// workspace's primary session (`sessions[0]`), matching the single-row and
// bulk archive paths, so the predicate keys off that session rather than any
// sibling. Snoozed-but-not-archived members are included (archiving the whole
// project should still sweep them in); members with no session are skipped.
export function archivableWorkspaces(group: SidebarGroup): Workspace[] {
  return group.workspaces
    .map((v) => v.workspace)
    .filter((ws) => {
      const primary = ws.sessions[0];
      return primary != null && primary.archived_at == null;
    });
}

// The nested `repo+group` axis (#1720). A repository header keeps its full
// repo-axis identity (`repo`), and inside it the same `group_path` buckets
// the user-group axis already computes show up as `subgroups`. This is a
// composition of the two existing builders, not a third bucketing pass:
// `repo` comes from `repoGroupToSidebarGroup`, `subgroups` from
// `buildSessionGroups` over that repo's own workspaces.
export interface NestedSidebarGroup {
  repo: SidebarGroup;
  subgroups: SidebarGroup[];
}

// Build the nested axis from the already-built repo-axis groups. Top-level
// ordering, appearance, synthetic Multi-repo / Scratch buckets, and per-repo
// collapse are inherited verbatim from the repo axis; only manual drag
// reorder is dropped (the nested axis has no manual order, like the group
// axis). Each repo's subgroups are the user-group split of just that repo's
// workspaces, so a workspace whose sessions span groups is sliced per
// subgroup exactly as the flat group axis does.
export function buildNestedSidebarGroups(
  repoGroups: RepoGroup[],
  opts: {
    idleDecayWindowMs: number;
    // Forwarded to the per-repo subgroup builder so subgroup rows honor the
    // selected sort mode. Top-level repo order is inherited from the repo
    // axis (already sorted by `useRepoGroups`), so it is not re-sorted here.
    sortMode: SidebarSortMode;
    isSubgroupCollapsed: (repoId: string, groupPath: string) => boolean;
  },
): NestedSidebarGroup[] {
  return repoGroups.map((repoGroup) => {
    const repo = repoGroupToSidebarGroup(repoGroup);
    const subgroups = buildSessionGroups(repoGroup.workspaces, {
      idleDecayWindowMs: opts.idleDecayWindowMs,
      sortMode: opts.sortMode,
      isCollapsed: (_groupId, groupPath) => opts.isSubgroupCollapsed(repo.id, groupPath),
    });
    return {
      repo: {
        ...repo,
        capabilities: { ...repo.capabilities, reorder: false },
      },
      subgroups,
    };
  });
}

// Nested-axis equivalent of `sidebarGroupHasLiveWorkspace`: true while any
// subgroup still has a live row, so an all-sunk repository block is not
// rendered as an empty header.
export function nestedSidebarGroupHasLiveWorkspace(group: NestedSidebarGroup): boolean {
  return group.subgroups.some(sidebarGroupHasLiveWorkspace);
}

// Nested-axis equivalent of `sidebarGroupShouldRender`: a pinned-but-empty
// repo has no subgroups (no sessions), so it would fail the live check, but
// its header must still render. See #2047.
export function nestedSidebarGroupShouldRender(group: NestedSidebarGroup): boolean {
  return group.repo.pinnedEmpty || group.subgroups.some(sidebarGroupShouldRender);
}

import type { ProjectInfo, RepoGroup, SessionResponse, Workspace, WorkspaceStatus } from "./types";
import { isSessionActive } from "./session";
import { compareNames, workspaceIsSunk } from "./sidebarSort";
import { MULTI_REPO_GROUP_ID, SCRATCH_GROUP_ID } from "../hooks/useRepoGroups";

// Bucket for sessions with no `group_path`; also a localStorage collapse key.
export const UNGROUPED_GROUP_ID = "__ungrouped__";

// Header affordances per grouping, gated here instead of by `kind` checks while rendering.
export interface SidebarGroupCapabilities {
  create: "repo" | "generic";
}

// Under My groups `sessions` is a per-group slice, so use `workspace.id` for actions and `key` only for render/DnD identity.
export interface SidebarWorkspaceView {
  key: string;
  workspace: Workspace;
}

export interface SidebarGroup {
  id: string;
  kind: "repo" | "sessionGroup";
  displayName: string;
  /** `owner@host` of the Repository's remote; repo groups only. */
  remoteOwnerKey: string | null;
  workspaces: SidebarWorkspaceView[];
  status: WorkspaceStatus;
  collapsed: boolean;
  capabilities: SidebarGroupCapabilities;
  repoPath?: string;
  /** Set when `kind === "sessionGroup"`. Empty string for Ungrouped. */
  groupPath?: string;
  /** Registered Projects for this repo path, pinned or not. Repo groups only. */
  registeredProjects: ProjectInfo[];
  pinned: boolean;
  /** Pinned with no live workspace. */
  pinnedEmpty: boolean;
}

function isSyntheticRepoGroup(id: string): boolean {
  return id === MULTI_REPO_GROUP_ID || id === SCRATCH_GROUP_ID;
}

// Synthetic Multi-repo and Scratch buckets keep the wizard create action.
export function repoGroupToSidebarGroup(group: RepoGroup): SidebarGroup {
  const synthetic = isSyntheticRepoGroup(group.id);
  // An unpinned Project gets the plain Project marker and no sessionless header.
  const pinned = !synthetic && group.registeredProjects.some((p) => p.pinned);
  return {
    id: group.id,
    kind: "repo",
    displayName: group.displayName,
    remoteOwnerKey: group.remoteOwnerKey,
    workspaces: group.workspaces.map((workspace) => ({
      key: workspace.id,
      workspace,
    })),
    status: group.status,
    collapsed: group.collapsed,
    capabilities: { create: synthetic ? "generic" : "repo" },
    repoPath: group.repoPath,
    registeredProjects: synthetic ? [] : group.registeredProjects,
    pinned,
    pinnedEmpty: pinned && group.workspaces.length === 0,
  };
}

function normalizeGroupPath(path: string | null | undefined): string {
  const trimmed = (path ?? "").trim();
  if (trimmed === "") return "";
  // "feature" and "feature/" are the same group.
  return trimmed.replace(/^\/+|\/+$/g, "");
}

function groupDisplayName(path: string): string {
  if (path === "") return "Ungrouped";
  // Flat rendering shows the full path, since sibling groups can share a leaf name.
  return path.split("/").join(" / ");
}

// Split workspaces by per-session `group_path`; a workspace spanning groups appears once per group.
export function buildSessionGroups(
  workspaces: Workspace[],
  opts: {
    idleDecayWindowMs: number;
    compareWorkspace: (a: Workspace, b: Workspace) => number;
    // `groupPath` is "" for Ungrouped, letting nested callers avoid the sentinel id.
    isCollapsed: (groupId: string, groupPath: string) => boolean;
    /** Every session in Ungrouped, whatever its Group. */
    ungrouped?: boolean;
  },
): SidebarGroup[] {
  const { compareWorkspace } = opts;
  const byGroup = new Map<string, SidebarWorkspaceView[]>();
  const order: string[] = [];

  for (const ws of workspaces) {
    const sessionsByGroup = new Map<string, SessionResponse[]>();
    for (const session of ws.sessions) {
      const gp = opts.ungrouped ? "" : normalizeGroupPath(session.group_path);
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
      remoteOwnerKey: null,
      workspaces: views,
      status: hasActive ? "active" : "idle",
      collapsed: opts.isCollapsed(id, gp),
      capabilities: { create: "generic" },
      groupPath: gp,
      registeredProjects: [],
      pinned: false,
      pinnedEmpty: false,
    });
  }

  groups.sort((a, b) => {
    if (a.id === UNGROUPED_GROUP_ID) return 1;
    if (b.id === UNGROUPED_GROUP_ID) return -1;
    return compareNames(a.displayName, b.displayName);
  });

  return groups;
}

export function sidebarGroupHasLiveWorkspace(group: SidebarGroup): boolean {
  return group.workspaces.some((v) => !workspaceIsSunk(v.workspace));
}

// A pinned-but-empty project still renders its header.
export function sidebarGroupShouldRender(group: SidebarGroup): boolean {
  return group.pinnedEmpty || sidebarGroupHasLiveWorkspace(group);
}

// Workspaces whose primary session (`sessions[0]`, the triage target) is not archived.
export function archivableWorkspaces(group: SidebarGroup): Workspace[] {
  return group.workspaces
    .map((v) => v.workspace)
    .filter((ws) => {
      const primary = ws.sessions[0];
      return primary != null && primary.archived_at == null;
    });
}

// A Repository header with its workspaces split into the user's Groups. Ungrouped rows come first and render
// straight under the header, as in the TUI, which also keeps Scratch sessions out of Groups.
export interface NestedSidebarGroup {
  repo: SidebarGroup;
  subgroups: SidebarGroup[];
}

export function buildNestedSidebarGroups(
  repoGroups: RepoGroup[],
  opts: {
    idleDecayWindowMs: number;
    compareWorkspace: (a: Workspace, b: Workspace) => number;
    isSubgroupCollapsed: (repoId: string, groupPath: string) => boolean;
  },
): NestedSidebarGroup[] {
  return repoGroups.map((repoGroup) => {
    const repo = repoGroupToSidebarGroup(repoGroup);
    const subgroups = buildSessionGroups(repoGroup.workspaces, {
      ungrouped: repoGroup.id === SCRATCH_GROUP_ID,
      idleDecayWindowMs: opts.idleDecayWindowMs,
      compareWorkspace: opts.compareWorkspace,
      isCollapsed: (_groupId, groupPath) => opts.isSubgroupCollapsed(repo.id, groupPath),
    });
    const ungrouped = subgroups.filter((g) => g.id === UNGROUPED_GROUP_ID);
    return { repo, subgroups: [...ungrouped, ...subgroups.filter((g) => g.id !== UNGROUPED_GROUP_ID)] };
  });
}

export function nestedSidebarGroupHasLiveWorkspace(group: NestedSidebarGroup): boolean {
  return group.subgroups.some(sidebarGroupHasLiveWorkspace);
}

export function nestedSidebarGroupShouldRender(group: NestedSidebarGroup): boolean {
  return group.repo.pinnedEmpty || group.subgroups.some(sidebarGroupShouldRender);
}

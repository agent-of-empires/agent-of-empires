import type { RepoColor } from "./repoAppearance";
import type { ProjectInfo, RepoGroup, SessionResponse, Workspace, WorkspaceStatus } from "./types";
import { isSessionActive } from "./session";
import {
  compareWorkspacesByPluginSort,
  compareWorkspacesForComputedSortMode,
  type PluginSortContext,
  type SidebarSortMode,
  workspaceIsSunk,
} from "./sidebarSort";
import { MULTI_REPO_GROUP_ID, SCRATCH_GROUP_ID } from "../hooks/useRepoGroups";

// Bucket for sessions with no `group_path`; also a localStorage collapse key.
export const UNGROUPED_GROUP_ID = "__ungrouped__";

// Bucket for repos with no resolvable remote owner; also a localStorage collapse key.
export const NO_ORG_GROUP_ID = "__no_org__";

// Header affordances per axis, gated here instead of by `kind` checks while rendering.
export interface SidebarGroupCapabilities {
  appearance: boolean;
  reorder: boolean;
  create: "repo" | "generic";
}

// In the group axis `sessions` is a per-group slice, so use `workspace.id` for actions and `key` only for render/DnD identity.
export interface SidebarWorkspaceView {
  key: string;
  workspace: Workspace;
}

export interface SidebarGroup {
  id: string;
  kind: "repo" | "sessionGroup" | "org";
  displayName: string;
  defaultDisplayName: string;
  alias: string | null;
  color: RepoColor | null;
  remoteOwner: string | null;
  workspaces: SidebarWorkspaceView[];
  status: WorkspaceStatus;
  collapsed: boolean;
  capabilities: SidebarGroupCapabilities;
  repoPath?: string;
  /** Set when `kind === "sessionGroup"`. Empty string for Ungrouped. */
  groupPath?: string;
  /** Saved projects for this repo path, pinned or not. Repo axis only. */
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
  // A saved but unpinned project gets no marker and no sessionless header.
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
    // `manual` falls back to last activity on this axis.
    sortMode: SidebarSortMode;
    pluginSort?: PluginSortContext;
    // `groupPath` is "" for Ungrouped, letting nested callers avoid the sentinel id.
    isCollapsed: (groupId: string, groupPath: string) => boolean;
  },
): SidebarGroup[] {
  const compareWorkspace = opts.pluginSort
    ? compareWorkspacesByPluginSort(opts.pluginSort)
    : compareWorkspacesForComputedSortMode(opts.sortMode);
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

// Repo-axis group with its workspaces split into user-group subgroups.
export interface NestedSidebarGroup {
  repo: SidebarGroup;
  subgroups: SidebarGroup[];
}

// Repo order, appearance and collapse come from the repo axis; subgroups split each repo's workspaces.
export function buildNestedSidebarGroups(
  repoGroups: RepoGroup[],
  opts: {
    idleDecayWindowMs: number;
    sortMode: SidebarSortMode;
    pluginSort?: PluginSortContext;
    isSubgroupCollapsed: (repoId: string, groupPath: string) => boolean;
  },
): NestedSidebarGroup[] {
  return repoGroups.map((repoGroup) => {
    const repo = repoGroupToSidebarGroup(repoGroup);
    const subgroups = buildSessionGroups(repoGroup.workspaces, {
      idleDecayWindowMs: opts.idleDecayWindowMs,
      sortMode: opts.sortMode,
      pluginSort: opts.pluginSort,
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

export function nestedSidebarGroupHasLiveWorkspace(group: NestedSidebarGroup): boolean {
  return group.subgroups.some(sidebarGroupHasLiveWorkspace);
}

export function nestedSidebarGroupShouldRender(group: NestedSidebarGroup): boolean {
  return group.repo.pinnedEmpty || group.subgroups.some(sidebarGroupShouldRender);
}

// A partition of repos by host-scoped remote owner.
export interface OrgNestedGroup {
  org: SidebarGroup;
  repos: SidebarGroup[];
}

// Repo properties and order come from the repo axis; collapse is keyed per org.
export function buildOrgGroups(
  repoGroups: RepoGroup[],
  opts: {
    isOrgCollapsed: (orgId: string) => boolean;
    isRepoCollapsed: (orgId: string, repoId: string) => boolean;
  },
): OrgNestedGroup[] {
  const byOrg = new Map<string, RepoGroup[]>();
  const order: string[] = [];

  for (const repoGroup of repoGroups) {
    const orgId = repoGroup.remoteOwnerKey ?? NO_ORG_GROUP_ID;
    const bucket = byOrg.get(orgId);
    if (bucket) {
      bucket.push(repoGroup);
    } else {
      byOrg.set(orgId, [repoGroup]);
      order.push(orgId);
    }
  }

  const result: OrgNestedGroup[] = order.map((orgId) => {
    const members = byOrg.get(orgId)!;
    const repos = members.map((repoGroup) => {
      const repo = repoGroupToSidebarGroup(repoGroup);
      return {
        ...repo,
        collapsed: opts.isRepoCollapsed(orgId, repo.id),
        capabilities: { ...repo.capabilities, reorder: false },
      };
    });
    const workspaces = repos.flatMap((r) => r.workspaces);
    const hasActive = repos.some((r) => r.status === "active");
    // `orgId` is the host-scoped key, so read the display owner off a member.
    const displayName = orgId === NO_ORG_GROUP_ID ? "No organization" : (members[0]?.remoteOwner ?? orgId);
    const org: SidebarGroup = {
      id: orgId,
      kind: "org",
      displayName,
      defaultDisplayName: displayName,
      alias: null,
      color: null,
      remoteOwner: orgId === NO_ORG_GROUP_ID ? null : displayName,
      workspaces,
      status: hasActive ? "active" : "idle",
      collapsed: opts.isOrgCollapsed(orgId),
      capabilities: { appearance: false, reorder: false, create: "generic" },
      registeredProjects: [],
      pinned: false,
      pinnedEmpty: false,
    };
    return { org, repos };
  });

  result.sort((a, b) => {
    if (a.org.id === NO_ORG_GROUP_ID) return 1;
    if (b.org.id === NO_ORG_GROUP_ID) return -1;
    return a.org.displayName.localeCompare(b.org.displayName);
  });

  return result;
}

export function orgNestedGroupShouldRender(group: OrgNestedGroup): boolean {
  return group.repos.some(sidebarGroupShouldRender);
}

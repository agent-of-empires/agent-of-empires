import { useMemo } from "react";
import type { ProjectInfo, Workspace, RepoGroup } from "../lib/types";
import { mergeRegisteredProjects, unpinnedSavedProjects } from "../lib/registeredProjects";
import { useCollapsedKeys } from "./useCollapsedKeys";
import { compareSortValues } from "../lib/pluginUi";
import {
  compareNames,
  type PluginSortContext,
  repoGroupLastActivityMs,
  repoGroupPluginSortValue,
  workspaceComparator,
  workspacesNewestMs,
} from "../lib/sidebarSort";
import type { SidebarSort } from "../lib/sidebarView";

export const MULTI_REPO_GROUP_ID = "__multi_repo__";
export const SCRATCH_GROUP_ID = "__scratch__";

function isMultiRepoWorkspace(ws: Workspace): boolean {
  return ws.sessions.some((s) => (s.workspace_repos?.length ?? 0) > 1);
}

function isScratchWorkspace(ws: Workspace): boolean {
  return ws.sessions.some((s) => s.scratch);
}

export const isSyntheticGroup = (id: string) => id === MULTI_REPO_GROUP_ID || id === SCRATCH_GROUP_ID;

const NONE: readonly never[] = [];

export interface RepoGroupOptions {
  sort: SidebarSort;
  /** The shared Custom order of workspaces. */
  workspaceOrdering?: readonly string[];
  /** The shared Custom order of Repository headers. */
  repositoryOrder?: readonly string[];
  projects?: readonly ProjectInfo[];
  pluginSort?: PluginSortContext;
  // False lists every registered Project in `savedProjects` (#5065).
  drawsRepoHeaders?: boolean;
}

export function useRepoGroups(
  workspaces: Workspace[],
  {
    sort,
    workspaceOrdering = NONE,
    repositoryOrder = NONE,
    projects = NONE,
    pluginSort,
    drawsRepoHeaders = true,
  }: RepoGroupOptions,
): {
  groups: RepoGroup[];
  savedProjects: RepoGroup[];
  toggleRepoCollapsed: (repoId: string) => void;
} {
  const { isCollapsed, toggle: toggleRepoCollapsed } = useCollapsedKeys("aoe-repo-collapsed-");

  const { groups, savedProjects } = useMemo(() => {
    const compareWorkspace = workspaceComparator(sort, workspaceOrdering, pluginSort);
    const sortWorkspaces = (list: Workspace[]) => [...list].sort(compareWorkspace);

    const byRepo = new Map<string, Workspace[]>();
    const multiRepo: Workspace[] = [];
    const scratch: Workspace[] = [];

    for (const ws of workspaces) {
      if (isScratchWorkspace(ws)) {
        scratch.push(ws);
        continue;
      }
      if (isMultiRepoWorkspace(ws)) {
        multiRepo.push(ws);
        continue;
      }
      const existing = byRepo.get(ws.projectPath);
      if (existing) existing.push(ws);
      else byRepo.set(ws.projectPath, [ws]);
    }

    const makeGroup = (id: string, list: Workspace[], displayName: string): RepoGroup => {
      const sorted = sortWorkspaces(list);
      const owner = isSyntheticGroup(id) ? null : sorted[0]?.sessions[0];
      return {
        id,
        repoPath: id,
        displayName,
        remoteOwner: owner?.remote_owner ?? null,
        remoteOwnerKey: owner?.remote_owner_key ?? null,
        workspaces: sorted,
        status: sorted.some((ws) => ws.status === "active") ? "active" : "idle",
        collapsed: isCollapsed(id),
        registeredProjects: [],
      };
    };

    const repoGroups: RepoGroup[] = [];
    for (const [repoPath, repoWorkspaces] of byRepo) {
      repoGroups.push(makeGroup(repoPath, repoWorkspaces, repoPath.split("/").pop() ?? repoPath));
    }
    if (multiRepo.length > 0) repoGroups.push(makeGroup(MULTI_REPO_GROUP_ID, multiRepo, "Multi-repo"));
    if (scratch.length > 0) repoGroups.push(makeGroup(SCRATCH_GROUP_ID, scratch, "Scratch"));

    const merged = mergeRegisteredProjects(repoGroups, [...projects], isCollapsed);

    const isRegisteredEmpty = (g: RepoGroup) => g.workspaces.length === 0 && g.registeredProjects.length > 0;

    const headerRank = new Map(repositoryOrder.map((id, i) => [id, i] as const));
    // Ties and headers never moved by hand fall back to newest session first.
    const newestFirst = (a: RepoGroup, b: RepoGroup) =>
      workspacesNewestMs(b.workspaces) - workspacesNewestMs(a.workspaces) || a.repoPath.localeCompare(b.repoPath);
    const compareHeaders = (a: RepoGroup, b: RepoGroup): number => {
      if (pluginSort) {
        const ae = isRegisteredEmpty(a);
        if (ae !== isRegisteredEmpty(b)) return ae ? 1 : -1;
        const cmp = compareSortValues(
          repoGroupPluginSortValue(a.workspaces, pluginSort),
          repoGroupPluginSortValue(b.workspaces, pluginSort),
          pluginSort.direction,
        );
        const ak = repoGroupLastActivityMs(a.workspaces);
        const bk = repoGroupLastActivityMs(b.workspaces);
        return cmp || (ak !== bk ? (bk > ak ? 1 : -1) : a.repoPath.localeCompare(b.repoPath));
      }
      if (sort === "name") return compareNames(a.displayName, b.displayName) || a.repoPath.localeCompare(b.repoPath);
      const ar = headerRank.get(a.id);
      const br = headerRank.get(b.id);
      if (ar === undefined || br === undefined) return ar === br ? newestFirst(a, b) : ar === undefined ? -1 : 1;
      return ar - br;
    };
    // Multi-repo and Scratch stay at the bottom, in that order, under every sort.
    const syntheticRank = (g: RepoGroup) => (g.id === MULTI_REPO_GROUP_ID ? 1 : g.id === SCRATCH_GROUP_ID ? 2 : 0);
    merged.sort((a, b) => syntheticRank(a) - syntheticRank(b) || compareHeaders(a, b));

    const savedProjects = unpinnedSavedProjects(repoGroups, [...projects], { drawsRepoHeaders });

    return { groups: merged, savedProjects };
  }, [workspaces, workspaceOrdering, repositoryOrder, sort, pluginSort, projects, isCollapsed, drawsRepoHeaders]);

  return {
    groups,
    savedProjects,
    toggleRepoCollapsed,
  };
}

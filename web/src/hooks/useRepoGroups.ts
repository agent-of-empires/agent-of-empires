import { useCallback, useMemo, useState } from "react";
import type { ProjectInfo, Workspace, RepoGroup } from "../lib/types";
import { mergeRegisteredProjects, unpinnedSavedProjects } from "../lib/registeredProjects";
import {
  applyRepoAppearanceUpdate,
  loadRepoAppearances,
  persistRepoAppearances,
  type RepoAppearanceUpdate,
} from "../lib/repoAppearance";
import { loadRepoGroupOrder, persistRepoGroupOrder } from "../lib/repoGroupOrder";
import { useCollapsedKeys } from "./useCollapsedKeys";
import { compareSortValues } from "../lib/pluginUi";
import {
  compareWorkspacesByAttention,
  compareWorkspacesByLastActivityDesc,
  compareWorkspacesByPluginSort,
  type PluginSortContext,
  repoGroupAttentionRank,
  repoGroupIsFavorited,
  repoGroupIsUrgent,
  repoGroupLastActivityMs,
  repoGroupPluginSortValue,
  workspaceTriageTier,
  type SidebarSortMode,
} from "../lib/sidebarSort";

export const MULTI_REPO_GROUP_ID = "__multi_repo__";
export const SCRATCH_GROUP_ID = "__scratch__";

function isMultiRepoWorkspace(ws: Workspace): boolean {
  return ws.sessions.some((s) => (s.workspace_repos?.length ?? 0) > 1);
}

function isScratchWorkspace(ws: Workspace): boolean {
  return ws.sessions.some((s) => s.scratch);
}

export function useRepoGroups(
  workspaces: Workspace[],
  workspaceOrdering: readonly string[] = [],
  sortMode: SidebarSortMode = "manual",
  projects: readonly ProjectInfo[] = [],
  pluginSort?: PluginSortContext,
): {
  groups: RepoGroup[];
  savedProjects: RepoGroup[];
  toggleRepoCollapsed: (repoId: string) => void;
  updateRepoAppearance: (repoId: string, update: RepoAppearanceUpdate) => void;
  reorderRepoGroups: (orderedGroupIds: string[]) => void;
} {
  const { isCollapsed, toggle: toggleRepoCollapsed } = useCollapsedKeys("aoe-repo-collapsed-");
  const [appearanceMap, setAppearanceMap] = useState(loadRepoAppearances);
  const [groupOrder, setGroupOrder] = useState<string[]>(loadRepoGroupOrder);

  const { groups, savedProjects } = useMemo(() => {
    const rank = new Map(workspaceOrdering.map((id, i) => [id, i] as const));
    const rankOf = (id: string) => rank.get(id) ?? Infinity;
    const groupRank = new Map(groupOrder.map((id, i) => [id, i] as const));
    const sortByRank = (list: Workspace[]) =>
      [...list].sort((a, b) => {
        const aTier = workspaceTriageTier(a);
        const bTier = workspaceTriageTier(b);
        if (aTier !== bTier) return aTier - bTier;
        // `Infinity - Infinity` is NaN, so compare explicitly and tie-break on id.
        const ar = rankOf(a.id);
        const br = rankOf(b.id);
        if (ar < br) return -1;
        if (ar > br) return 1;
        return a.id.localeCompare(b.id);
      });
    const pluginCompare = pluginSort ? compareWorkspacesByPluginSort(pluginSort) : null;
    const sortWorkspaces = (list: Workspace[]) => {
      if (pluginCompare) {
        return [...list].sort(pluginCompare);
      }
      if (sortMode === "attention") {
        return [...list].sort(compareWorkspacesByAttention);
      }
      if (sortMode === "lastActivity") {
        return [...list].sort(compareWorkspacesByLastActivityDesc);
      }
      return sortByRank(list);
    };

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

    const isSyntheticGroup = (id: string) => id === MULTI_REPO_GROUP_ID || id === SCRATCH_GROUP_ID;
    const makeGroup = (id: string, list: Workspace[], defaultDisplayName: string): RepoGroup => {
      const sorted = sortWorkspaces(list);
      const appearance = appearanceMap[id];
      const owner = isSyntheticGroup(id) ? null : sorted[0]?.sessions[0];
      return {
        id,
        repoPath: id,
        displayName: appearance?.alias ?? defaultDisplayName,
        defaultDisplayName,
        alias: appearance?.alias ?? null,
        color: appearance?.color ?? null,
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

    const merged = mergeRegisteredProjects(repoGroups, [...projects], {
      alias: (repoPath) => appearanceMap[repoPath]?.alias ?? null,
      color: (repoPath) => appearanceMap[repoPath]?.color ?? null,
      collapsed: isCollapsed,
    });

    const isRegisteredEmpty = (g: RepoGroup) => g.workspaces.length === 0 && g.registeredProjects.length > 0;

    const autoCompare = pluginSort
      ? (a: RepoGroup, b: RepoGroup) =>
          compareSortValues(
            repoGroupPluginSortValue(a.workspaces, pluginSort),
            repoGroupPluginSortValue(b.workspaces, pluginSort),
            pluginSort.direction,
          )
      : sortMode === "attention"
        ? (a: RepoGroup, b: RepoGroup) => {
            const au = repoGroupIsUrgent(a.workspaces);
            const bu = repoGroupIsUrgent(b.workspaces);
            if (au !== bu) return au ? -1 : 1;
            const ar = repoGroupAttentionRank(a.workspaces);
            const br = repoGroupAttentionRank(b.workspaces);
            if (ar !== br) return ar - br;
            const af = repoGroupIsFavorited(a.workspaces);
            const bf = repoGroupIsFavorited(b.workspaces);
            return af === bf ? 0 : af ? -1 : 1;
          }
        : sortMode === "lastActivity"
          ? () => 0
          : null;

    merged.sort((a, b) => {
      if (autoCompare) {
        // Synthetic groups sink below real repos, and repos with no live workspace below those.
        if (a.id === SCRATCH_GROUP_ID) return 1;
        if (b.id === SCRATCH_GROUP_ID) return -1;
        if (a.id === MULTI_REPO_GROUP_ID) return 1;
        if (b.id === MULTI_REPO_GROUP_ID) return -1;
        const ae = isRegisteredEmpty(a);
        const be = isRegisteredEmpty(b);
        if (ae !== be) return ae ? 1 : -1;
        const cmp = autoCompare(a, b);
        if (cmp !== 0) return cmp;
        const ak = repoGroupLastActivityMs(a.workspaces);
        const bk = repoGroupLastActivityMs(b.workspaces);
        if (ak !== bk) return bk - ak;
        return a.repoPath.localeCompare(b.repoPath);
      }
      const SYNTHETIC_BOTTOM = Number.MAX_SAFE_INTEGER;
      const fallbackRank = (g: RepoGroup) =>
        isSyntheticGroup(g.id) ? SYNTHETIC_BOTTOM : isRegisteredEmpty(g) ? SYNTHETIC_BOTTOM - 1 : -1;
      const keyOf = (g: RepoGroup) => groupRank.get(g.id) ?? fallbackRank(g);
      const ka = keyOf(a);
      const kb = keyOf(b);
      if (ka !== kb) return ka - kb;
      if (ka === SYNTHETIC_BOTTOM) {
        if (a.id === MULTI_REPO_GROUP_ID) return -1;
        if (b.id === MULTI_REPO_GROUP_ID) return 1;
        return 0;
      }
      const am = Math.min(...a.workspaces.map((w) => rankOf(w.id)));
      const bm = Math.min(...b.workspaces.map((w) => rankOf(w.id)));
      if (am !== bm) return am - bm;
      return a.repoPath.localeCompare(b.repoPath);
    });

    const savedProjects = unpinnedSavedProjects(repoGroups, [...projects], {
      alias: (repoPath) => appearanceMap[repoPath]?.alias ?? null,
      color: (repoPath) => appearanceMap[repoPath]?.color ?? null,
    });

    return { groups: merged, savedProjects };
  }, [workspaces, workspaceOrdering, sortMode, pluginSort, projects, isCollapsed, appearanceMap, groupOrder]);

  const updateRepoAppearance = useCallback((repoId: string, update: RepoAppearanceUpdate) => {
    setAppearanceMap((prev) => {
      const next = applyRepoAppearanceUpdate(prev, repoId, update);
      persistRepoAppearances(next);
      return next;
    });
  }, []);

  const reorderRepoGroups = useCallback((orderedGroupIds: string[]) => {
    setGroupOrder(orderedGroupIds);
    persistRepoGroupOrder(orderedGroupIds);
  }, []);

  return {
    groups,
    savedProjects,
    toggleRepoCollapsed,
    updateRepoAppearance,
    reorderRepoGroups,
  };
}

// Sidebar grouping, sort, and ordering for the dashboard's workspaces. The grouping, the sort and the Custom
// order are the Daemon's, shared with every Client; a plugin sort is this browser's alone.

import { useCallback, useMemo, useState } from "react";
import type { RuntimeCursor, SidebarView, WorkspaceOrderingPatch } from "../../lib/api";
import { reportFeatureUse } from "../../lib/featureUse";
import { buildSortValueMap, pluginSortSpecs } from "../../lib/pluginUi";
import { usePluginUiEntries } from "../../lib/pluginUiContext";
import { moveId, orderStartingWith } from "../../lib/sidebarReorder";
import { workspaceComparator, type PluginSortContext } from "../../lib/sidebarSort";
import { GROUP_BY_FEATURE, SORT_FEATURE, type SidebarGroupBy, type SidebarSort } from "../../lib/sidebarView";
import type { ProjectInfo, Workspace } from "../../lib/types";
import { useAttentionJump } from "../useAttentionJump";
import { useNestedSidebarGroups } from "../useNestedSidebarGroups";
import { isSyntheticGroup, useRepoGroups } from "../useRepoGroups";
import { useSessionGroups } from "../useSessionGroups";

export type PluginSortRef = { pluginId: string; entryId: string };

interface Options {
  workspaces: Workspace[];
  projects: ProjectInfo[];
  view: SidebarView;
  workspaceOrdering: string[];
  repositoryOrder: string[];
  changeOrdering: (patch: WorkspaceOrderingPatch) => Promise<RuntimeCursor | null>;
  changeSidebarView: (patch: Partial<SidebarView>, after?: Promise<unknown>) => Promise<void>;
}

export function useSidebarModel({
  workspaces,
  projects,
  view,
  workspaceOrdering,
  repositoryOrder,
  changeOrdering,
  changeSidebarView,
}: Options) {
  const pluginUiEntries = usePluginUiEntries();
  // Plugin entries die with the Daemon, so the pick is not persisted. A ref whose entry is gone is inert and the
  // shared sort applies; if the entry reappears the pick resumes.
  const [pluginSortRef, setPluginSortRef] = useState<PluginSortRef | null>(null);
  const activePluginSort = useMemo(
    () =>
      (pluginSortRef &&
        pluginSortSpecs(pluginUiEntries).find(
          (s) => s.pluginId === pluginSortRef.pluginId && s.entryId === pluginSortRef.entryId,
        )) ||
      null,
    [pluginUiEntries, pluginSortRef],
  );
  const pluginSort = useMemo<PluginSortContext | undefined>(
    () =>
      activePluginSort
        ? {
            direction: activePluginSort.direction,
            values: buildSortValueMap(pluginUiEntries, activePluginSort.pluginId, activePluginSort.column),
          }
        : undefined,
    [activePluginSort, pluginUiEntries],
  );

  const groupBy = view.group_by;
  const sort = view.sort;
  const compareWorkspace = useMemo(
    () => workspaceComparator(sort, workspaceOrdering, pluginSort),
    [sort, workspaceOrdering, pluginSort],
  );
  const repo = useRepoGroups(workspaces, {
    sort,
    workspaceOrdering,
    repositoryOrder,
    projects,
    pluginSort,
    drawsRepoHeaders: groupBy === "project",
  });
  const session = useSessionGroups(workspaces, compareWorkspace);
  const nested = useNestedSidebarGroups(repo.groups, compareWorkspace);

  // The rendered groups in display order, so jump-to-next and drags follow what the user sees.
  const displayGroups = useMemo(
    () => (groupBy === "project" ? nested.groups.flatMap((n) => n.subgroups) : session.groups),
    [groupBy, nested.groups, session.groups],
  );
  const attentionJump = useAttentionJump(displayGroups);

  const selectGroupBy = useCallback(
    (next: SidebarGroupBy) => {
      reportFeatureUse(GROUP_BY_FEATURE[next]);
      if (next !== groupBy) void changeSidebarView({ group_by: next });
    },
    [groupBy, changeSidebarView],
  );
  const selectSort = useCallback(
    (next: SidebarSort) => {
      reportFeatureUse(SORT_FEATURE[next]);
      setPluginSortRef(null);
      if (next !== sort) void changeSidebarView({ sort: next });
    },
    [sort, changeSidebarView],
  );
  const selectPluginSort = useCallback((ref: PluginSortRef) => {
    reportFeatureUse("sort_plugin");
    setPluginSortRef(ref);
  }, []);
  const resetCustomOrder = useCallback(() => void changeOrdering({ order: [], repositories: [] }), [changeOrdering]);

  // Under Name a drag switches to Custom starting from the order on screen, so nothing else moves.
  const frozen = sort === "name" && !pluginSort;
  const onScreenOrder = useCallback(
    (): WorkspaceOrderingPatch => ({
      order: orderStartingWith(
        displayGroups.flatMap((g) => g.workspaces.map((v) => v.workspace.id)),
        workspaceOrdering,
      ),
      ...(groupBy === "project" && {
        repositories: repo.groups.map((g) => g.id).filter((id) => !isSyntheticGroup(id)),
      }),
    }),
    [displayGroups, workspaceOrdering, groupBy, repo.groups],
  );
  const commit = useCallback(
    (patch: WorkspaceOrderingPatch) => {
      const write = changeOrdering(patch);
      if (frozen) void changeSidebarView({ sort: "custom" }, write);
    },
    [frozen, changeOrdering, changeSidebarView],
  );

  /** Moves a row next to `overId`, after it when the drag went down. */
  const moveWorkspace = useCallback(
    (activeId: string, overId: string, after: boolean) => {
      reportFeatureUse("row_reorder");
      const base = frozen ? onScreenOrder() : { order: workspaceOrdering };
      commit({ ...base, order: moveId(base.order ?? [], activeId, overId, after) });
    },
    [frozen, workspaceOrdering, onScreenOrder, commit],
  );

  /** Moves a Repository header next to `overId`, after it when the drag went down. */
  const moveRepository = useCallback(
    (activeId: string, overId: string, after: boolean) => {
      const headers = repo.groups.map((g) => g.id);
      const repositories = moveId(headers, activeId, overId, after).filter((id) => !isSyntheticGroup(id));
      commit(frozen ? { ...onScreenOrder(), repositories } : { repositories });
    },
    [repo.groups, frozen, onScreenOrder, commit],
  );

  return {
    groupBy,
    sort,
    pluginSortRef: activePluginSort ? pluginSortRef : null,
    selectGroupBy,
    selectSort,
    selectPluginSort,
    resetCustomOrder,
    repoGroups: repo.groups,
    savedProjects: repo.savedProjects,
    toggleRepoCollapsed: repo.toggleRepoCollapsed,
    groups: session.groups,
    toggleGroupCollapsed: session.toggleGroupCollapsed,
    nestedGroups: nested.groups,
    toggleSubgroupCollapsed: nested.toggleSubgroupCollapsed,
    moveWorkspace,
    moveRepository,
    attentionJump,
  };
}

// Sidebar grouping, sort, and ordering for the dashboard's workspaces. The grouping, the sort and the Custom
// order are the Daemon's, shared with every Client; a plugin sort is this browser's alone.

import { useCallback, useMemo, useRef, useState } from "react";
import type { RuntimeCursor, SidebarView, WorkspaceOrderingPatch } from "../../lib/api";
import { reportFeatureUse } from "../../lib/featureUse";
import { buildSortValueMap, pluginSortSpecs } from "../../lib/pluginUi";
import { usePluginUiEntries } from "../../lib/pluginUiContext";
import { reportInfo, toastBus } from "../../lib/toastBus";
import { keepUnshown, moveId, orderStartingWith } from "../../lib/sidebarReorder";
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
  orderingSaving: () => boolean;
  changeSidebarView: (patch: Partial<SidebarView>, after?: Promise<RuntimeCursor | null>) => Promise<void>;
}

export function useSidebarModel({
  workspaces,
  projects,
  view,
  workspaceOrdering,
  repositoryOrder,
  changeOrdering,
  orderingSaving,
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

  // Like the TUI, a move is refused while the last one is saving, so it never builds on a write that may fail.
  const switching = useRef(false);
  const refuseWhileSaving = useCallback(() => {
    if (!switching.current && !orderingSaving()) return false;
    reportInfo("The last move is still saving");
    return true;
  }, [orderingSaving]);

  // One level of Undo for the order changes that rewrite more than the dragged item, offered once the write
  // lands. The next order or sort change drops it.
  const undoOffer = useRef<{ seq: number; dismiss?: () => void }>({ seq: 0 });
  const dropUndo = useCallback(() => {
    const offer = undoOffer.current;
    offer.seq += 1;
    offer.dismiss?.();
    offer.dismiss = undefined;
  }, []);
  const offerUndo = useCallback(
    (message: string, write: Promise<RuntimeCursor | null>, restoreName: boolean) => {
      const offer = undoOffer.current;
      const seq = offer.seq;
      const previous = { order: workspaceOrdering, repositories: repositoryOrder };
      void write.then((receipt) => {
        if (!receipt || offer.seq !== seq) return;
        offer.dismiss = toastBus.handler?.infoAction(message, "Undo", () => {
          if (refuseWhileSaving()) return false;
          dropUndo();
          const undone = changeOrdering(previous);
          if (restoreName) void changeSidebarView({ sort: "name" }, undone);
        });
      });
    },
    [workspaceOrdering, repositoryOrder, refuseWhileSaving, dropUndo, changeOrdering, changeSidebarView],
  );

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
      if (next === sort) return;
      dropUndo();
      void changeSidebarView({ sort: next });
    },
    [sort, dropUndo, changeSidebarView],
  );
  const selectPluginSort = useCallback((ref: PluginSortRef) => {
    reportFeatureUse("sort_plugin");
    setPluginSortRef(ref);
  }, []);

  // Under Name a drag switches to Custom starting from the order on screen, so nothing else moves.
  const frozen = sort === "name" && !pluginSort;
  const onScreenOrder = useCallback(
    (): WorkspaceOrderingPatch => ({
      order: orderStartingWith(
        displayGroups.flatMap((g) => g.workspaces.map((v) => v.workspace.id)),
        workspaceOrdering,
      ),
      ...(groupBy === "project" && {
        repositories: keepUnshown(
          repo.groups.map((g) => g.id).filter((id) => !isSyntheticGroup(id)),
          repositoryOrder,
        ),
      }),
    }),
    [displayGroups, workspaceOrdering, groupBy, repo.groups, repositoryOrder],
  );
  const commit = useCallback(
    (patch: WorkspaceOrderingPatch) => {
      dropUndo();
      const write = changeOrdering(patch);
      if (!frozen) return;
      offerUndo("Sorted by Custom now", write, true);
      switching.current = true;
      void changeSidebarView({ sort: "custom" }, write).finally(() => (switching.current = false));
    },
    [frozen, dropUndo, changeOrdering, offerUndo, changeSidebarView],
  );

  const resetCustomOrder = useCallback(() => {
    dropUndo();
    offerUndo("Custom order reset", changeOrdering({ order: [], repositories: [] }), false);
  }, [dropUndo, changeOrdering, offerUndo]);

  /** Moves a row next to `overId`, after it when the drag went down. */
  const moveWorkspace = useCallback(
    (activeId: string, overId: string, after: boolean) => {
      if (refuseWhileSaving()) return;
      reportFeatureUse("row_reorder");
      // A reset lists nothing until the next poll, and moving next to an unlisted row needs it listed.
      const listed = workspaceOrdering.includes(activeId) && workspaceOrdering.includes(overId);
      const base = frozen ? onScreenOrder() : { order: listed ? workspaceOrdering : onScreenOrder().order };
      commit({ ...base, order: moveId(base.order ?? [], activeId, overId, after) });
    },
    [refuseWhileSaving, frozen, workspaceOrdering, onScreenOrder, commit],
  );

  /** Moves a Repository header next to `overId`, after it when the drag went down. */
  const moveRepository = useCallback(
    (activeId: string, overId: string, after: boolean) => {
      if (refuseWhileSaving()) return;
      const headers = repo.groups.map((g) => g.id);
      const moved = moveId(headers, activeId, overId, after).filter((id) => !isSyntheticGroup(id));
      const repositories = keepUnshown(moved, repositoryOrder);
      commit(frozen ? { ...onScreenOrder(), repositories } : { repositories });
    },
    [refuseWhileSaving, repo.groups, repositoryOrder, frozen, onScreenOrder, commit],
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

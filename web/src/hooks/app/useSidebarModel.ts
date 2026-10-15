// Sidebar grouping, sort, and ordering for the dashboard's workspaces.

import { useCallback, useMemo, useState } from "react";
import { updateWorkspaceOrdering } from "../../lib/api";
import { buildSortValueMap, pluginSortSpecs } from "../../lib/pluginUi";
import { usePluginAttention, usePluginUiEntries } from "../../lib/pluginUiContext";
import { repoGroupToSidebarGroup } from "../../lib/sidebarGroups";
import type { PluginSortContext, SidebarSortMode } from "../../lib/sidebarSort";
import type { ProjectInfo, Workspace } from "../../lib/types";
import { useAttentionJump } from "../useAttentionJump";
import { useNestedSidebarGroups } from "../useNestedSidebarGroups";
import { useOrgGroups } from "../useOrgGroups";
import { useRepoGroups } from "../useRepoGroups";
import { useSessionGroups } from "../useSessionGroups";
import { useSidebarAxis, useSidebarSortMode } from "../useSidebarPrefs";

interface Options {
  workspaces: Workspace[];
  workspaceOrdering: string[];
  setWorkspaceOrdering: (order: string[]) => void;
  markLocalOrderingUpdate: () => void;
  projects: ProjectInfo[];
}

export function useSidebarModel({
  workspaces,
  workspaceOrdering,
  setWorkspaceOrdering,
  markLocalOrderingUpdate,
  projects,
}: Options) {
  const [sortMode, setSortMode] = useSidebarSortMode();
  const [axis, setAxis] = useSidebarAxis();

  const pluginUiEntries = usePluginUiEntries();
  const pluginAttention = usePluginAttention();
  const [pluginSortRef, setPluginSortRef] = useState<{ pluginId: string; entryId: string } | null>(null);
  const pluginSort = useMemo<PluginSortContext | undefined>(() => {
    const spec =
      pluginSortRef &&
      pluginSortSpecs(pluginUiEntries).find(
        (s) => s.pluginId === pluginSortRef.pluginId && s.entryId === pluginSortRef.entryId,
      );
    return spec
      ? { direction: spec.direction, values: buildSortValueMap(pluginUiEntries, spec.pluginId, spec.column) }
      : undefined;
  }, [pluginUiEntries, pluginSortRef]);

  const selectSortMode = useCallback(
    (mode: SidebarSortMode) => {
      setPluginSortRef(null);
      setSortMode(mode);
    },
    [setSortMode],
  );

  const repo = useRepoGroups(workspaces, workspaceOrdering, sortMode, projects, pluginSort, pluginAttention);
  const session = useSessionGroups(workspaces, sortMode, pluginSort, pluginAttention);
  const nested = useNestedSidebarGroups(repo.groups, sortMode, pluginSort, pluginAttention);
  const org = useOrgGroups(repo.groups);

  const groups = useMemo(
    () => (axis === "group" ? session.groups : repo.groups.map(repoGroupToSidebarGroup)),
    [axis, session.groups, repo.groups],
  );

  const reorderWorkspaces = useCallback(
    (newOrder: string[]) => {
      setWorkspaceOrdering(newOrder);
      markLocalOrderingUpdate();
      void updateWorkspaceOrdering(newOrder);
    },
    [setWorkspaceOrdering, markLocalOrderingUpdate],
  );

  // Sessions in sidebar order, plus those needing attention, for the jump-to-attention shortcut.
  const attentionJump = useAttentionJump(groups);

  return {
    sortMode,
    selectSortMode,
    axis,
    setAxis,
    pluginSortRef,
    setPluginSortRef,
    groups,
    toggleGroup: axis === "group" ? session.toggleGroupCollapsed : repo.toggleRepoCollapsed,
    nestedGroups: nested.groups,
    toggleSubgroupCollapsed: nested.toggleSubgroupCollapsed,
    orgGroups: org.groups,
    toggleOrgCollapsed: org.toggleOrgCollapsed,
    toggleOrgRepoCollapsed: org.toggleRepoCollapsed,
    savedProjects: repo.savedProjects,
    updateRepoAppearance: repo.updateRepoAppearance,
    reorderRepoGroups: repo.reorderRepoGroups,
    reorderWorkspaces,
    attentionJump,
  };
}

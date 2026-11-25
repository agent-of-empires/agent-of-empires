// Sidebar grouping, sort, and ordering for the dashboard's workspaces. The grouping, the sort and the Custom
// order are the Daemon's, shared with every Client; a plugin sort is this browser's alone.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { RuntimeCursor, SidebarView, WorkspaceOrderingPatch } from "../../lib/api";
import { reportFeatureUse } from "../../lib/featureUse";
import {
  arrangeGroups,
  groupMove,
  groupPathComparator,
  mergeGroupOrder,
  parentPath,
  type GroupMove,
  type GroupOrder,
} from "../../lib/groupOrder";
import { buildSortValueMap, pluginSortSpecs } from "../../lib/pluginUi";
import { usePluginUiEntries } from "../../lib/pluginUiContext";
import { reportInfo, toastBus } from "../../lib/toastBus";
import { keepUnshown, moveId, orderStartingWith } from "../../lib/sidebarReorder";
import { compareNames, workspaceComparator, type PluginSortContext } from "../../lib/sidebarSort";
import { GROUP_BY_FEATURE, SORT_FEATURE, type SidebarGroupBy, type SidebarSort } from "../../lib/sidebarView";
import type { ProjectInfo, Workspace } from "../../lib/types";
import type { SharedRead } from "../useSessions";
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
  groupOrder: GroupOrder;
  moveGroup: (
    profile: string,
    path: string,
    move: GroupMove,
    after?: Promise<RuntimeCursor | null>,
  ) => Promise<RuntimeCursor | null>;
  changeOrdering: (patch: WorkspaceOrderingPatch) => Promise<RuntimeCursor | null>;
  orderingSaving: () => boolean;
  changeSidebarView: (
    patch: Partial<SidebarView>,
    after?: Promise<RuntimeCursor | null>,
  ) => Promise<RuntimeCursor | null>;
  onSharedRead: (listener: (read: SharedRead) => void) => () => void;
}

/** The shared values an order change wrote, or that its Undo puts back. */
interface Shared {
  order?: string[];
  repositories?: string[];
  groups?: GroupOrder;
  sort?: SidebarSort;
}

interface UndoOffer {
  seq: number;
  dismiss?: () => void;
  /** What the change left, from its receipt on, and which of those values a read has shown since. */
  watch?: { receipt: RuntimeCursor; left: Shared; seen: Set<keyof Shared> };
}

const sameList = (a: readonly string[], b: readonly string[]) => a.length === b.length && a.every((x, i) => x === b[i]);

/** Whether the paths `now` and `wrote` share stand among their siblings in the same order in both. */
function sameSiblingOrder(now: readonly string[], wrote: readonly string[]): boolean {
  const kept = now.filter((p) => wrote.includes(p));
  const expected = wrote.filter((p) => now.includes(p));
  const under = (list: string[], parent: string) => list.filter((p) => parentPath(p) === parent);
  return [...new Set(expected.map(parentPath))].every((parent) =>
    sameList(under(kept, parent), under(expected, parent)),
  );
}

/**
 * Whether `read` still holds what `left` names, adopting the Daemon's copy of each value the first time a read
 * shows it: the Daemon lists rows the written order lacks first and keeps Groups the sidebar does not show, so the
 * first read is checked only for what the change wrote.
 */
function stillLeft(watch: NonNullable<UndoOffer["watch"]>, read: SharedRead): boolean {
  const { left, seen } = watch;
  for (const field of ["order", "repositories", "groups", "sort"] as const) {
    const wrote = left[field];
    const now = read[field];
    if (wrote === undefined || now === undefined) continue;
    const first = !seen.has(field);
    seen.add(field);
    let same: boolean;
    if (field === "groups") {
      const groups = now as GroupOrder;
      same = Object.entries(wrote as GroupOrder).every(([p, paths]) =>
        (first ? sameSiblingOrder : sameList)(groups[p] ?? [], paths),
      );
      if (first && same) left.groups = Object.fromEntries(Object.keys(wrote).map((p) => [p, groups[p] ?? []]));
    } else if (field === "sort") {
      same = now === wrote;
    } else {
      const list = now as string[];
      const tail =
        first && field === "order" ? list.slice(Math.max(0, list.length - (wrote as string[]).length)) : list;
      same = sameList(tail, wrote as string[]);
      left[field] = list;
    }
    if (!same) return false;
  }
  return true;
}

/** The newest receipt once every write lands, or null when one fails. */
async function settleAll(writes: Promise<RuntimeCursor | null>[]): Promise<RuntimeCursor | null> {
  const receipts = await Promise.all(writes);
  let newest: RuntimeCursor | null = null;
  for (const receipt of receipts) {
    if (!receipt) return null;
    if (!newest || receipt.revision > newest.revision) newest = receipt;
  }
  return newest;
}

export function useSidebarModel({
  workspaces,
  projects,
  view,
  workspaceOrdering,
  repositoryOrder,
  groupOrder,
  moveGroup,
  changeOrdering,
  orderingSaving,
  changeSidebarView,
  onSharedRead,
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
  // Under Custom, Group headers follow the Daemon's Group order; under Name they stay A to Z.
  const comparePaths = useMemo(
    () => (sort === "custom" ? groupPathComparator(mergeGroupOrder(groupOrder)) : undefined),
    [sort, groupOrder],
  );
  const session = useSessionGroups(workspaces, compareWorkspace, comparePaths);
  const nested = useNestedSidebarGroups(repo.groups, compareWorkspace, comparePaths);

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
  // lands. The next local change drops it, and so does a read showing another Client changed what it wrote.
  const undoOffer = useRef<UndoOffer>({ seq: 0 });
  const dropUndo = useCallback(() => {
    const offer = undoOffer.current;
    offer.seq += 1;
    offer.dismiss?.();
    offer.dismiss = undefined;
    offer.watch = undefined;
  }, []);
  useEffect(
    () =>
      // eslint-disable-next-line react-you-might-not-need-an-effect/no-pass-data-to-parent -- subscribes to reads
      onSharedRead((read) => {
        const watch = undoOffer.current.watch;
        if (!watch) return;
        if (read.cursor.epoch !== watch.receipt.epoch) return dropUndo();
        if (read.cursor.revision >= watch.receipt.revision && !stillLeft(watch, read)) dropUndo();
      }),
    [onSharedRead, dropUndo],
  );

  /** Moves each profile's Group order from `current` back to `previous`, after `after`. */
  const restoreGroups = useCallback(
    (current: GroupOrder, previous: GroupOrder, after: Promise<RuntimeCursor | null>) =>
      Object.entries(previous).flatMap(([profile, paths]) =>
        arrangeGroups(current[profile] ?? [], [], groupPathComparator(paths)).map(({ path, move }) =>
          moveGroup(profile, path, move, after),
        ),
      ),
    [moveGroup],
  );

  /** Writes `previous` back over a change that left the Groups as `groups`; resolves null when a write failed. */
  const putBack = useCallback(
    (previous: Shared, groups: GroupOrder, receipt: RuntimeCursor) => {
      const { groups: previousGroups, sort: previousSort, ...patch } = previous;
      const ordered = Object.keys(patch).length > 0 ? changeOrdering(patch) : Promise.resolve(receipt);
      const undone = settleAll([ordered, ...restoreGroups(groups, previousGroups ?? {}, ordered)]);
      return previousSort ? changeSidebarView({ sort: previousSort }, undone) : undone;
    },
    [changeOrdering, restoreGroups, changeSidebarView],
  );

  const offerUndo = useCallback(
    (message: string, write: Promise<RuntimeCursor | null>, wrote: Shared, previous: Shared) => {
      const offer = undoOffer.current;
      const seq = offer.seq;
      void write.then((receipt) => {
        if (!receipt || offer.seq !== seq) return;
        const watch = { receipt, left: wrote, seen: new Set<keyof Shared>() };
        offer.watch = watch;
        offer.dismiss = toastBus.handler?.infoAction(message, "Undo", () => {
          if (refuseWhileSaving()) return false;
          dropUndo();
          // While the offer stands, the Groups are as the change left them.
          void putBack(previous, watch.left.groups ?? {}, receipt);
        });
      });
    },
    [refuseWhileSaving, dropUndo, putBack],
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
  /** Each profile's Group moves that keep the headers in the name order Name shows them in, and how many profiles
   *  the headers show. */
  const onScreenGroupMoves = useCallback(() => {
    const shown = new Map<string, string[]>();
    for (const g of displayGroups) {
      if (!g.groupPath) continue;
      for (const s of g.workspaces.flatMap((v) => v.workspace.sessions)) {
        shown.set(s.profile, [...(shown.get(s.profile) ?? []), g.groupPath]);
      }
    }
    const moves = [...shown].flatMap(([profile, paths]) =>
      arrangeGroups(groupOrder[profile] ?? [], paths, compareNames).map((m) => ({ profile, ...m })),
    );
    return { moves, profiles: shown.size };
  }, [displayGroups, groupOrder]);
  /** Whether the Group headers keep their place on screen once each profile's Group order is `after`. */
  const keepsHeaderOrder = useCallback(
    (after: GroupOrder) => {
      const compare = groupPathComparator(mergeGroupOrder(after));
      const lists = groupBy === "project" ? nested.groups.map((n) => n.subgroups) : [session.groups];
      return lists.every((list) => {
        const paths = list.flatMap((g) => (g.groupPath ? [g.groupPath] : []));
        return paths.every((p, i) => i === 0 || compare(paths[i - 1]!, p) < 0);
      });
    },
    [groupBy, nested.groups, session.groups],
  );
  const commit = useCallback(
    (patch: WorkspaceOrderingPatch) => {
      if (!frozen) {
        dropUndo();
        void changeOrdering(patch);
        return;
      }
      const { moves, profiles: shownProfiles } = onScreenGroupMoves();
      const profiles = [...new Set(moves.map((m) => m.profile))];
      const groupsAfter = Object.fromEntries(
        profiles.map((p) => [p, moves.findLast((m) => m.profile === p)!.move.order]),
      );
      // Custom lists each profile's Groups after the previous profile's, which may not keep a merged name order.
      if (shownProfiles > 1 && !keepsHeaderOrder({ ...groupOrder, ...groupsAfter })) {
        reportInfo("Pick Custom first: Groups of several profiles can't keep this order");
        return;
      }
      dropUndo();
      const offer = undoOffer.current;
      const seq = offer.seq;
      const write = changeOrdering(patch);
      const wrote: Shared = { ...patch, sort: "custom" };
      const previous: Shared = {
        ...(patch.order && { order: workspaceOrdering }),
        ...(patch.repositories && { repositories: repositoryOrder }),
        sort: "name",
      };
      if (moves.length > 0) {
        wrote.groups = groupsAfter;
        previous.groups = Object.fromEntries(profiles.map((p) => [p, groupOrder[p] ?? []]));
      }
      const writes = [write, ...moves.map((m) => moveGroup(m.profile, m.path, m.move, write))];
      switching.current = true;
      const switched = changeSidebarView({ sort: "custom" }, settleAll(writes));
      offerUndo("Sorted by Custom now", switched, wrote, previous);
      // The orders and the sort land together: when the switch fails after a write landed, the orders go back.
      void Promise.all([switched, ...writes])
        .then(([receipt, ...receipts]) => {
          const landed = receipts.find((r) => r !== null);
          if (receipt || !landed || offer.seq !== seq) return;
          return putBack({ ...previous, sort: undefined }, wrote.groups ?? {}, landed).then((undone) =>
            reportInfo(
              undone
                ? "Couldn't switch to Custom, so nothing moved"
                : "Couldn't switch to Custom; part of the move saved",
            ),
          );
        })
        .finally(() => (switching.current = false));
    },
    [
      frozen,
      dropUndo,
      changeOrdering,
      onScreenGroupMoves,
      keepsHeaderOrder,
      workspaceOrdering,
      repositoryOrder,
      groupOrder,
      moveGroup,
      offerUndo,
      putBack,
      changeSidebarView,
    ],
  );

  const resetCustomOrder = useCallback(() => {
    if (refuseWhileSaving()) return;
    dropUndo();
    const cleared = { order: [], repositories: [] };
    offerUndo(
      "Custom order reset",
      changeOrdering(cleared),
      { ...cleared },
      {
        order: workspaceOrdering,
        repositories: repositoryOrder,
      },
    );
  }, [refuseWhileSaving, dropUndo, changeOrdering, offerUndo, workspaceOrdering, repositoryOrder]);

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
      // Multi-repo and Scratch stay at the bottom.
      if (isSyntheticGroup(activeId) || isSyntheticGroup(overId) || refuseWhileSaving()) return;
      const headers = repo.groups.map((g) => g.id);
      const moved = moveId(headers, activeId, overId, after).filter((id) => !isSyntheticGroup(id));
      const repositories = keepUnshown(moved, repositoryOrder);
      commit(frozen ? { ...onScreenOrder(), repositories } : { repositories });
    },
    [refuseWhileSaving, repo.groups, repositoryOrder, frozen, onScreenOrder, commit],
  );

  /** Moves a Group header next to its sibling `overPath`, after it when the drag went down. */
  const moveGroupHeader = useCallback(
    (activePath: string, overPath: string, after: boolean) => {
      if (sort !== "custom") {
        reportInfo("Pick Custom to arrange groups");
        return;
      }
      if (refuseWhileSaving() || activePath === overPath) return;
      const profilesOf = (g: (typeof displayGroups)[number]) =>
        g.workspaces.flatMap((v) => v.workspace.sessions.map((s) => s.profile));
      const profiles = new Set(displayGroups.filter((g) => g.groupPath === activePath).flatMap(profilesOf));
      // The Daemon orders each profile's Groups apart, so a header merging two has no one place to go.
      const [profile] = profiles;
      if (profiles.size !== 1 || profile === undefined) {
        reportInfo("Groups move only within one profile");
        return;
      }
      if (parentPath(overPath) !== parentPath(activePath)) {
        reportInfo("Couldn't move the group there");
        return;
      }
      const order = groupOrder[profile] ?? [];
      const shown = displayGroups
        .filter((g) => g.groupPath && profilesOf(g).includes(profile))
        .map((g) => g.groupPath!);
      if (!order.includes(overPath) && !shown.includes(overPath)) {
        reportInfo("Groups move only within one profile");
        return;
      }
      const move = groupMove(order, shown, activePath, overPath, after);
      if (!move) return;
      dropUndo();
      void moveGroup(profile, activePath, move).then((receipt) => {
        if (!receipt) reportInfo("Couldn't move the group there");
      });
    },
    [sort, refuseWhileSaving, displayGroups, groupOrder, dropUndo, moveGroup],
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
    moveGroupHeader,
    attentionJump,
  };
}

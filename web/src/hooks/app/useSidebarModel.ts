// Sidebar grouping, sort, and ordering for the dashboard's workspaces. The grouping, the sort and the Custom
// order are the Daemon's, shared with every Client; a plugin sort is this browser's alone.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  clearWebPluginSortOnUnload,
  type RuntimeCursor,
  type SidebarView,
  type SidebarViewPatch,
  type WorkspaceOrderingPatch,
  type WriteResult,
} from "../../lib/api";
import { reportFeatureUse } from "../../lib/featureUse";
import {
  arrangeGroups,
  groupMove,
  groupPathComparator,
  mergeGroupOrder,
  parentPath,
  restoreGroups,
  type GroupMove,
  type GroupOrder,
} from "../../lib/groupOrder";
import { buildSortValueMap, pluginSortSpecs } from "../../lib/pluginUi";
import { usePluginUiEntries } from "../../lib/pluginUiContext";
import { reportInfo, toastBus } from "../../lib/toastBus";
import { keepUnshown, moveId, orderStartingWith } from "../../lib/sidebarReorder";
import { workspaceComparator, type PluginSortContext } from "../../lib/sidebarSort";
import {
  GROUP_BY_FEATURE,
  peerViewChangeNotice,
  SORT_FEATURE,
  type SidebarGroupBy,
  type SidebarSort,
} from "../../lib/sidebarView";
import type { PathOrder } from "../../lib/sidebarGroups";
import type { ProjectInfo, Workspace } from "../../lib/types";
import type { SharedRead } from "../useSessions";
import { useAttentionJump } from "../useAttentionJump";
import { useNestedSidebarGroups } from "../useNestedSidebarGroups";
import { isSyntheticGroup, useRepoGroups } from "../useRepoGroups";
import { useSessionGroups } from "../useSessionGroups";

export type PluginSortRef = { pluginId: string; entryId: string };

// The plugin sort marker this tab last set on the Daemon, so a reload that lost the pick can clear it. Per tab, as
// the pick is; another browser that set the same label is indistinguishable and is cleared too.
const MARKER_KEY = "aoe-web-plugin-sort-marker";
function rememberMarker(label: string | null) {
  try {
    if (label) sessionStorage.setItem(MARKER_KEY, label);
    else sessionStorage.removeItem(MARKER_KEY);
  } catch {
    // Storage unavailable: the unload clear still runs.
  }
}
function rememberedMarker(): string | null {
  try {
    return sessionStorage.getItem(MARKER_KEY);
  } catch {
    return null;
  }
}

interface Options {
  workspaces: Workspace[];
  projects: ProjectInfo[];
  view: SidebarView;
  /** The view the Daemon last served, without this browser's picks still in flight; null before the first read. */
  polledView: SidebarView | null;
  /** False on a read-only Daemon or under CityHall, which refuse view writes. */
  viewWritable: boolean;
  workspaceOrdering: string[];
  repositoryOrder: string[];
  groupOrder: GroupOrder;
  moveGroup: (
    profile: string,
    path: string,
    move: GroupMove,
    after?: Promise<WriteResult>,
    base?: string[],
    baseSort?: SidebarSort,
  ) => Promise<WriteResult>;
  changeOrdering: (patch: WorkspaceOrderingPatch) => Promise<WriteResult>;
  orderingSaving: () => boolean;
  changeSidebarView: (
    patch: SidebarViewPatch,
    after?: Promise<WriteResult>,
    baseSort?: SidebarSort,
  ) => Promise<WriteResult>;
  onSharedRead: (listener: (read: SharedRead) => void) => () => void;
  /** Reads the Daemon now. */
  refresh: () => void;
}

type OrderingBase = NonNullable<WorkspaceOrderingPatch["base"]>;
/** The row order on screen, with the Repository header order when the sidebar shows those headers. */
type ShownOrder = { order: string[]; repositories?: string[] };

/** The shared values an order change wrote, or that its Undo puts back. */
interface Shared {
  order?: string[];
  repositories?: string[];
  groups?: GroupOrder;
  sort?: SidebarSort;
}

/** What putting a change back needs: the values it replaced, the stored orders and the Group order it left. */
interface Undoable {
  previous: Shared;
  base: OrderingBase;
  groups: GroupOrder;
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

/** The newest receipt once every write lands, else the gravest failure: a conflict, then one that may have landed. */
async function settleAll(writes: Promise<WriteResult>[]): Promise<WriteResult> {
  const results = await Promise.all(writes);
  for (const failure of ["conflict", "uncertain", "rejected"] as const) {
    const failed = results.find((r) => r.failure === failure);
    if (failed) return failed;
  }
  return results.reduce((a, b) => (b.receipt!.revision > a.receipt!.revision ? b : a));
}

export function useSidebarModel({
  workspaces,
  projects,
  view,
  polledView,
  viewWritable,
  workspaceOrdering,
  repositoryOrder,
  groupOrder,
  moveGroup,
  changeOrdering,
  orderingSaving,
  changeSidebarView,
  onSharedRead,
  refresh,
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
  // Under Custom, Group headers follow the Daemon's Group order, under Newest first their newest session, and under
  // Name they stay A to Z.
  const comparePaths = useMemo<PathOrder | undefined>(
    () => (sort === "custom" ? groupPathComparator(mergeGroupOrder(groupOrder)) : sort === "newest" ? sort : undefined),
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

  // Says why the list moved when another Client kind changed the view, and only when the rows on screen moved. Only a
  // change between two Daemon reads is named, so a refused local pick falling back is not blamed on a peer.
  const shownOrder = useMemo(
    () => displayGroups.map((g) => `${g.id}:${g.workspaces.map((v) => v.key).join(",")}`).join("|"),
    [displayGroups],
  );
  const lastShown = useRef({ view, polled: polledView, order: shownOrder });
  useEffect(() => {
    const last = lastShown.current;
    // The baseline is the last read shown on screen, so a read held behind this browser's own write is compared
    // with what was on screen once the list adopts it.
    const shown =
      polledView && polledView.group_by === view.group_by && polledView.sort === view.sort ? polledView : null;
    lastShown.current = { view, polled: shown ?? last.polled, order: shownOrder };
    const moved = last.order !== "" && last.order !== shownOrder;
    const notice = moved ? peerViewChangeNotice(last.view, last.polled, shown) : null;
    if (notice) reportInfo(notice);
  }, [view, polledView, shownOrder]);

  // Like the TUI, a move is refused while the last one is saving, so it never builds on a write that may fail.
  const switching = useRef(false);
  const refuseWhileSaving = useCallback(() => {
    if (!switching.current && !orderingSaving()) return false;
    reportInfo("The last move is still saving");
    return true;
  }, [orderingSaving]);

  // One level of Undo for a drag that switched the sort to Custom, offered once the write lands. The next local
  // change drops it, and so do a read showing another Client changed what it wrote and this model's teardown.
  const undoOffer = useRef<UndoOffer>({ seq: 0 });
  const dropUndo = useCallback(() => {
    const offer = undoOffer.current;
    offer.seq += 1;
    offer.dismiss?.();
    offer.dismiss = undefined;
    offer.watch = undefined;
  }, []);
  useEffect(() => {
    const unsubscribe = onSharedRead((read) => {
      const watch = undoOffer.current.watch;
      if (!watch) return;
      if (read.cursor.epoch !== watch.receipt.epoch) return dropUndo();
      if (read.cursor.revision >= watch.receipt.revision && !stillLeft(watch, read)) dropUndo();
    });
    return () => {
      unsubscribe();
      dropUndo();
    };
  }, [onSharedRead, dropUndo]);

  /**
   * Writes back what `change` replaced, with the Groups now `groups`. The orders go first, carrying the stored orders
   * the change left, so the Daemon refuses them once another Client changed them and nothing else is sent. Every
   * write carries the Group order it replaces, and an Undo, which restores the sort, also the sort.
   */
  const putBack = useCallback(
    (change: Undoable, groups: GroupOrder) => {
      const { groups: previousGroups, sort: previousSort, ...lists } = change.previous;
      const owned = previousSort !== undefined;
      const profiles = Object.keys(previousGroups ?? {});
      const ordered = changeOrdering({
        ...lists,
        base: change.base,
        ...(owned && { base_sort: "custom" }),
        ...(profiles.length > 0 && { base_groups: Object.fromEntries(profiles.map((p) => [p, groups[p] ?? []])) }),
      });
      const moves = Object.entries(previousGroups ?? {}).flatMap(([profile, paths]) => {
        let base = groups[profile] ?? [];
        return restoreGroups(base, paths, change.groups[profile] ?? []).map(({ path, move }) => {
          const write = moveGroup(profile, path, move, ordered, base, owned ? "custom" : undefined);
          base = move.order;
          return write;
        });
      });
      const undone = settleAll([ordered, ...moves]);
      return previousSort ? settleAll([undone, changeSidebarView({ sort: previousSort }, undone, "custom")]) : undone;
    },
    [changeOrdering, moveGroup, changeSidebarView],
  );

  const offerUndo = useCallback(
    (message: string, write: Promise<WriteResult>, wrote: Shared, change: Undoable) => {
      const offer = undoOffer.current;
      const seq = offer.seq;
      void write.then(({ receipt }) => {
        if (!receipt || offer.seq !== seq) return;
        const watch = { receipt, left: wrote, seen: new Set<keyof Shared>() };
        offer.watch = watch;
        offer.dismiss = toastBus.handler?.infoAction(message, "Undo", () => {
          if (refuseWhileSaving()) return false;
          dropUndo();
          // While the offer stands, the Groups are as the change left them.
          void putBack(change, watch.left.groups ?? {}).then(({ failure }) => {
            if (!failure) return;
            refresh();
            reportInfo(
              failure === "conflict"
                ? "Another Client changed the order, so Undo kept it"
                : "Couldn't undo; part of the change may have saved",
            );
          });
        });
      });
    },
    [refuseWhileSaving, dropUndo, putBack, refresh],
  );

  const selectGroupBy = useCallback(
    (next: SidebarGroupBy) => {
      reportFeatureUse(GROUP_BY_FEATURE[next]);
      if (next !== groupBy) void changeSidebarView({ group_by: next });
    },
    [groupBy, changeSidebarView],
  );
  // The plugin sort marker the other Clients show, as this browser last set it, or undefined after a write failed.
  // It follows the active plugin sort, so a plugin that goes away clears it, and the page clears it as it unloads.
  const markerSent = useRef<string | null | undefined>(null);
  const wantedRef = useRef<string | null>(null);
  const sendMarker = useCallback(
    (label: string | null) => {
      if (!viewWritable || label === markerSent.current) return;
      markerSent.current = label;
      if (label) rememberMarker(label);
      void changeSidebarView({ web_plugin_sort: label }).then((result) => {
        // A newer pick owns the marker now.
        if (markerSent.current !== label) return;
        if (result.receipt === null) markerSent.current = undefined;
        else if (label === null) rememberMarker(null);
      });
    },
    [viewWritable, changeSidebarView],
  );
  const wantedMarker = useMemo(() => {
    const label = pluginSortRef ? activePluginSort?.label : undefined;
    return label?.trim() ? [...label].slice(0, 64).join("") : null;
  }, [pluginSortRef, activePluginSort]);
  useEffect(() => {
    wantedRef.current = wantedMarker;
    sendMarker(wantedMarker);
  }, [wantedMarker, sendMarker]);
  // A failed marker write is retried on the next read, with the plugin sort picked then.
  useEffect(() => {
    const unsubscribe = onSharedRead(() => {
      if (markerSent.current === undefined) sendMarker(wantedRef.current);
    });
    return unsubscribe;
  }, [onSharedRead, sendMarker]);
  useEffect(() => {
    const onHide = () => {
      if (markerSent.current !== null) clearWebPluginSortOnUnload();
    };
    window.addEventListener("pagehide", onHide);
    return () => window.removeEventListener("pagehide", onHide);
  }, []);
  // A reload starts with no plugin sort, so a marker this tab set before it, and that the Daemon still shows, is stale.
  const staleChecked = useRef(false);
  const clearStaleMarker = useCallback(
    (loaded: boolean, daemonMarker: string | undefined) => {
      if (!loaded || staleChecked.current) return;
      staleChecked.current = true;
      const stale = rememberedMarker();
      if (!stale || markerSent.current !== null) return;
      if (!viewWritable || daemonMarker !== stale) {
        rememberMarker(null);
        return;
      }
      // Unknown until the clear lands, so a failed one is retried and the tab's record kept until then.
      markerSent.current = undefined;
      sendMarker(null);
    },
    [viewWritable, sendMarker],
  );
  const loaded = polledView !== null;
  const daemonMarker = polledView?.web_plugin_sort;
  useEffect(() => clearStaleMarker(loaded, daemonMarker), [loaded, daemonMarker, clearStaleMarker]);

  // A web patch that sets the sort also clears the plugin sort marker, on the Daemon too.
  const selectSort = useCallback(
    (next: SidebarSort) => {
      reportFeatureUse(SORT_FEATURE[next]);
      setPluginSortRef(null);
      if (next === sort) {
        sendMarker(null);
        return;
      }
      dropUndo();
      const write = changeSidebarView({ sort: next });
      // The Daemon clears the marker with a web sort; if that write fails, the next read retries the clear.
      if (viewWritable && markerSent.current !== null) {
        markerSent.current = null;
        void write.then((result) => {
          if (markerSent.current !== null) return;
          if (result.receipt === null) markerSent.current = undefined;
          else rememberMarker(null);
        });
      }
    },
    [sort, viewWritable, dropUndo, sendMarker, changeSidebarView],
  );
  const selectPluginSort = useCallback((ref: PluginSortRef) => {
    reportFeatureUse("sort_plugin");
    setPluginSortRef(ref);
  }, []);

  // Under Name or Newest first a drag switches to Custom starting from the order on screen, so nothing else moves.
  const frozen = sort !== "custom" && !pluginSort;
  const onScreenOrder = useCallback(
    (): ShownOrder => ({
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
  /** Each profile's Group paths in the order on screen, a parent before its first descendant. */
  const onScreenGroupPaths = useCallback(() => {
    const shown = new Map<string, string[]>();
    for (const g of displayGroups) {
      if (!g.groupPath) continue;
      const parts = g.groupPath.split("/");
      const lineage = parts.map((_, i) => parts.slice(0, i + 1).join("/"));
      for (const s of g.workspaces.flatMap((v) => v.workspace.sessions)) {
        const paths = shown.get(s.profile) ?? [];
        paths.push(...lineage.filter((p) => !paths.includes(p)));
        shown.set(s.profile, paths);
      }
    }
    return shown;
  }, [displayGroups]);
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
  /** Commits `patch` under Custom. */
  const commit = useCallback(
    (patch: WorkspaceOrderingPatch) => {
      dropUndo();
      void changeOrdering(patch);
    },
    [dropUndo, changeOrdering],
  );
  /** Under Name or Newest first, commits `patch` with the Group order on screen and the switch to Custom, with
   *  `header`, a Group header drag, on top of them. */
  const switchToCustom = useCallback(
    (patch: ShownOrder, header?: { profile: string; path: string; over: string; after: boolean }) => {
      const shown = onScreenGroupPaths();
      const moves = [...shown].flatMap(([profile, paths]) =>
        arrangeGroups(groupOrder[profile] ?? [], paths, groupPathComparator(paths)).map((m) => ({ profile, ...m })),
      );
      const groupsAfter: GroupOrder = Object.fromEntries(
        [...new Set(moves.map((m) => m.profile))].map((p) => [p, moves.findLast((m) => m.profile === p)!.move.order]),
      );
      // Custom lists each profile's Groups after the previous profile's, and in one order under every Repository,
      // which may not keep the headers where they are.
      if (!keepsHeaderOrder({ ...groupOrder, ...groupsAfter })) {
        reportInfo("Pick Custom first: the Groups can't keep this order");
        return;
      }
      if (header) {
        const arranged = groupsAfter[header.profile] ?? groupOrder[header.profile] ?? [];
        const move = groupMove(arranged, shown.get(header.profile) ?? [], header.path, header.over, header.after);
        if (!move) return;
        moves.push({ profile: header.profile, path: header.path, move });
        groupsAfter[header.profile] = move.order;
      }
      dropUndo();
      const offer = undoOffer.current;
      const seq = offer.seq;
      const write = changeOrdering(patch);
      const wrote: Shared = { ...patch, sort: "custom" };
      const previous: Shared = {
        order: workspaceOrdering,
        ...(patch.repositories && { repositories: repositoryOrder }),
        sort,
      };
      if (moves.length > 0) {
        wrote.groups = groupsAfter;
        previous.groups = Object.fromEntries(Object.keys(groupsAfter).map((p) => [p, groupOrder[p] ?? []]));
      }
      const writes = [write, ...moves.map((m) => moveGroup(m.profile, m.path, m.move, write))];
      switching.current = true;
      // Refused once a peer picked another sort since the drag began.
      const switched = changeSidebarView({ sort: "custom" }, settleAll(writes), sort);
      // The Daemon stores a list as written, so `base` is what `write` leaves.
      const change: Undoable = {
        previous,
        base: { order: patch.order, repositories: patch.repositories ?? repositoryOrder },
        groups: groupsAfter,
      };
      offerUndo("Switched to Custom", switched, wrote, change);
      // The orders and the sort land together: when the switch fails after a write landed, the orders go back.
      void Promise.all([switched, Promise.all(writes)])
        .then(([result, results]) => {
          if (result.receipt || offer.seq !== seq) return;
          // A write that may have landed leaves no known state to put back, so show what the Daemon holds.
          if ([result, ...results].some((r) => r.failure === "uncertain")) {
            refresh();
            reportInfo("Couldn't confirm the switch to Custom; the list shows what saved");
            return;
          }
          if (!results.some((r) => r.receipt)) {
            reportInfo("Couldn't switch to Custom, so nothing moved");
            return;
          }
          // Each profile's Groups as its moves that landed in turn left them.
          const left: GroupOrder = {};
          moves.forEach((m, i) => {
            if (left[m.profile] === undefined) left[m.profile] = groupOrder[m.profile] ?? [];
            const landed =
              results[i + 1]?.receipt &&
              moves.slice(0, i).every((p, j) => p.profile !== m.profile || results[j + 1]?.receipt);
            if (landed) left[m.profile] = m.move.order;
          });
          return putBack({ ...change, previous: { ...previous, sort: undefined } }, left).then(({ failure }) => {
            if (failure) refresh();
            reportInfo(
              !failure
                ? "Couldn't switch to Custom, so nothing moved"
                : failure === "conflict"
                  ? "Couldn't switch to Custom; another Client's newer order stays"
                  : "Couldn't switch to Custom; part of the move saved",
            );
          });
        })
        .finally(() => (switching.current = false));
    },
    [
      dropUndo,
      changeOrdering,
      onScreenGroupPaths,
      keepsHeaderOrder,
      sort,
      workspaceOrdering,
      repositoryOrder,
      groupOrder,
      moveGroup,
      offerUndo,
      putBack,
      changeSidebarView,
      refresh,
    ],
  );

  /** Moves a row next to `overId`, after it when the drag went down. */
  const moveWorkspace = useCallback(
    (activeId: string, overId: string, after: boolean) => {
      if (refuseWhileSaving()) return;
      reportFeatureUse("row_reorder");
      // Moving next to a row the order does not list yet needs it listed.
      if (frozen) {
        const shown = onScreenOrder();
        switchToCustom({ ...shown, order: moveId(shown.order, activeId, overId, after) });
        return;
      }
      const listed = workspaceOrdering.includes(activeId) && workspaceOrdering.includes(overId);
      commit({ order: moveId(listed ? workspaceOrdering : onScreenOrder().order, activeId, overId, after) });
    },
    [refuseWhileSaving, frozen, workspaceOrdering, onScreenOrder, switchToCustom, commit],
  );

  /** Moves a Repository header next to `overId`, after it when the drag went down. */
  const moveRepository = useCallback(
    (activeId: string, overId: string, after: boolean) => {
      // Multi-repo and Scratch stay at the bottom.
      if (isSyntheticGroup(activeId) || isSyntheticGroup(overId) || refuseWhileSaving()) return;
      const headers = repo.groups.map((g) => g.id);
      const moved = moveId(headers, activeId, overId, after).filter((id) => !isSyntheticGroup(id));
      const repositories = keepUnshown(moved, repositoryOrder);
      if (frozen) switchToCustom({ ...onScreenOrder(), repositories });
      else commit({ repositories });
    },
    [refuseWhileSaving, repo.groups, repositoryOrder, frozen, onScreenOrder, switchToCustom, commit],
  );

  /** Moves a Group header next to its sibling `overPath`, after it when the drag went down. Under Repository,
   *  `repoId` is the header's repository, whose Groups alone decide its profile and shown siblings. */
  const moveGroupHeader = useCallback(
    (activePath: string, overPath: string, after: boolean, repoId?: string) => {
      if (pluginSort) {
        reportInfo("Pick Custom to arrange groups");
        return;
      }
      if (refuseWhileSaving() || activePath === overPath) return;
      const scope =
        repoId === undefined ? displayGroups : (nested.groups.find((n) => n.repo.id === repoId)?.subgroups ?? []);
      const profilesOf = (g: (typeof scope)[number]) =>
        g.workspaces.flatMap((v) => v.workspace.sessions.map((s) => s.profile));
      const profiles = new Set(scope.filter((g) => g.groupPath === activePath).flatMap(profilesOf));
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
      const shown = scope.filter((g) => g.groupPath && profilesOf(g).includes(profile)).map((g) => g.groupPath!);
      if (!order.includes(overPath) && !shown.includes(overPath)) {
        reportInfo("Groups move only within one profile");
        return;
      }
      if (frozen) {
        switchToCustom(onScreenOrder(), { profile, path: activePath, over: overPath, after });
        return;
      }
      const move = groupMove(order, shown, activePath, overPath, after);
      if (!move) return;
      dropUndo();
      void moveGroup(profile, activePath, move).then(({ receipt }) => {
        if (!receipt) reportInfo("Couldn't move the group there");
      });
    },
    [
      pluginSort,
      refuseWhileSaving,
      displayGroups,
      nested.groups,
      groupOrder,
      frozen,
      switchToCustom,
      onScreenOrder,
      dropUndo,
      moveGroup,
    ],
  );

  return {
    groupBy,
    sort,
    pluginSortRef: activePluginSort ? pluginSortRef : null,
    selectGroupBy,
    selectSort,
    selectPluginSort,
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

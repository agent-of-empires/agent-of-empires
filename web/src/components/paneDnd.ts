import { createContext, useContext } from "react";

import type { DockLocation } from "../lib/panes";

/** Drag payloads. */
export interface PaneTabData {
  type: "pane-tab";
  dock: DockLocation;
  group: number;
}
export interface GroupDropData {
  type: "pane-group";
  dock: DockLocation;
  group: number;
}
/** A split half over a group body: dropping here lifts the tab into a fresh
 *  group before or after the hovered one. */
export interface SplitDropData {
  type: "pane-split";
  dock: DockLocation;
  group: number;
  side: "before" | "after";
}
export interface EmptyDockDropData {
  type: "pane-empty-dock";
  dock: DockLocation;
}
export type DockDropData = GroupDropData | SplitDropData | EmptyDockDropData;

/** The live insertion point while a pane tab is dragged. */
export interface DropTarget {
  dock: DockLocation;
  group: number;
  index?: number;
  newGroup?: boolean;
}

/** Source address of the dragged tab (its dock + group), so a within-group
 *  no-op reorder can be skipped. */
export interface DragSource {
  dock: DockLocation;
  group: number;
}

export interface PaneDndState {
  activeTab: string | null;
  source: DragSource | null;
  dropTarget: DropTarget | null;
}

export const PaneDndStateContext = createContext<PaneDndState>({
  activeTab: null,
  source: null,
  dropTarget: null,
});

/** Docks read this to show their destination ring, insertion marker, and the
 *  split drop zones (mounted only while a tab is dragged). */
export function usePaneDnd(): PaneDndState {
  return useContext(PaneDndStateContext);
}

/** One rendered group's visible tab ids, keyed by its persisted group index. */
export interface RenderGroup {
  group: number;
  tabs: string[];
}

/** The droppable the pointer is over, reduced to the bits placement needs. */
export interface PlacementOver {
  type: "pane-tab" | "pane-group" | "pane-split" | "pane-empty-dock";
  dock: DockLocation;
  /** Persisted group index of the hovered group (0 for an empty-dock zone). */
  group: number;
  /** Split side (only meaningful when `type` is "pane-split"). */
  side?: "before" | "after";
  /** The hovered tab id (only meaningful when `type` is "pane-tab"). */
  tabId: string;
  /** Pointer is past the hovered tab's center, so insert after it. */
  after: boolean;
}

/** Where a dragged tab lands. */
export function resolvePlacement(
  over: PlacementOver,
  draggedId: string,
  groupsByDock: Record<DockLocation, RenderGroup[]>,
): DropTarget {
  if (over.type === "pane-split") {
    return { dock: over.dock, group: over.side === "after" ? over.group + 1 : over.group, newGroup: true };
  }
  if (over.type === "pane-empty-dock") {
    return { dock: over.dock, group: 0, newGroup: true };
  }
  const grp = groupsByDock[over.dock].find((g) => g.group === over.group);
  const full = grp?.tabs ?? [];
  const base = full.filter((id) => id !== draggedId);
  if (over.type !== "pane-tab") return { dock: over.dock, group: over.group, index: base.length };
  // Dropping a tab onto its own tab is a no-op: keep its current slot rather than appending (base has the dragged
  // tab filtered out, so its index would otherwise resolve to the end).
  if (over.tabId === draggedId) {
    const currentIndex = full.indexOf(draggedId);
    return { dock: over.dock, group: over.group, index: currentIndex >= 0 ? currentIndex : base.length };
  }
  const overIndex = base.indexOf(over.tabId);
  if (overIndex < 0) return { dock: over.dock, group: over.group, index: base.length };
  return { dock: over.dock, group: over.group, index: overIndex + (over.after ? 1 : 0) };
}

interface Rect {
  left: number;
  width: number;
}

/** Horizontal center of a rect, or null when the rect is missing (dnd-kit hands
 *  a null translated rect before the first move). */
export function centerX(rect: Rect | null | undefined): number | null {
  return rect ? rect.left + rect.width / 2 : null;
}

/** True when the dragged tab's center has passed the hovered tab's center, so the drop should insert after it
 *  rather than before. */
export function pointerInsertsAfter(activeRect: Rect | null | undefined, overRect: Rect | null | undefined): boolean {
  const a = centerX(activeRect);
  const o = centerX(overRect);
  return a !== null && o !== null && a > o;
}

/** Whether a resolved drop is worth persisting: a split or a cross-group/dock move always is, but a within-group
 *  reorder onto the tab's own slot is a no-op to skip. */
export function shouldApplyPlacement(
  groupsByDock: Record<DockLocation, RenderGroup[]>,
  tabId: string,
  target: DropTarget,
  source: DragSource | null,
): boolean {
  if (target.newGroup) return true;
  if (!source || target.dock !== source.dock || target.group !== source.group) return true;
  const grp = groupsByDock[target.dock].find((g) => g.group === target.group);
  const from = grp ? grp.tabs.indexOf(tabId) : -1;
  return from >= 0 && from !== target.index;
}

/** Translate an insertion index in a group's *visible* tab list to the index in its *full* persisted list. */
export function visibleToFullIndex(
  fullBase: string[],
  visibleIndex: number,
  isVisible: (id: string) => boolean,
): number {
  const visibleSlots = fullBase.map((id, index) => ({ id, index })).filter(({ id }) => isVisible(id));
  return visibleIndex >= visibleSlots.length ? fullBase.length : visibleSlots[visibleIndex]!.index;
}

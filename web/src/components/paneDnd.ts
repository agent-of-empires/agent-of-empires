import { createContext, useContext } from "react";

import type { DockLocation } from "../lib/panes";

/** Drag payloads. A tab carries the dock it currently lives in; the two dock
 *  droppables (the rendered dock body and the empty-dock landing zone) carry
 *  their location. onDragEnd branches on `type`, never on the id shape. */
export interface PaneTabData {
  type: "pane-tab";
  dock: DockLocation;
}
export interface DockDropData {
  type: "pane-dock" | "pane-empty-dock";
  dock: DockLocation;
}

/** The live insertion point while a pane tab is dragged: which dock and the
 *  index it would land at within that dock (after the tab is removed from its
 *  source). Null when there is no valid target. */
export interface DropTarget {
  dock: DockLocation;
  index: number;
}

export interface PaneDndState {
  activeTab: string | null;
  sourceDock: DockLocation | null;
  dropTarget: DropTarget | null;
}

export const PaneDndStateContext = createContext<PaneDndState>({
  activeTab: null,
  sourceDock: null,
  dropTarget: null,
});

/** Dock reads this to show its destination ring and (cross-dock only) its
 *  insertion marker. Within-dock order is conveyed by the sortable shift, so
 *  the marker is suppressed when the target dock is the source dock. */
export function usePaneDnd(): PaneDndState {
  return useContext(PaneDndStateContext);
}

/** The droppable the pointer is over, reduced to the bits placement needs. */
export interface PlacementOver {
  type: "pane-tab" | "pane-dock" | "pane-empty-dock";
  dock: DockLocation;
  /** The hovered tab id (only meaningful when `type` is "pane-tab"). */
  tabId: string;
  /** Pointer is past the hovered tab's center, so insert after it. */
  after: boolean;
}

/** Where a dragged tab lands: the destination dock and the index in that dock's
 *  visible tab list *after* the dragged tab is removed. Dropping on a tab
 *  inserts before or after it; dropping on a dock body or empty-dock zone
 *  appends. Pure so the drag handler stays a thin adapter over the event. */
export function resolvePlacement(
  over: PlacementOver,
  draggedId: string,
  tabsByDock: Record<DockLocation, string[]>,
): DropTarget {
  const base = tabsByDock[over.dock].filter((id) => id !== draggedId);
  if (over.type !== "pane-tab") return { dock: over.dock, index: base.length };
  const overIndex = base.indexOf(over.tabId);
  if (overIndex < 0) return { dock: over.dock, index: base.length };
  return { dock: over.dock, index: overIndex + (over.after ? 1 : 0) };
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

/** True when the dragged tab's center has passed the hovered tab's center, so
 *  the drop should insert after it rather than before. False if either rect is
 *  unknown, biasing toward inserting before. */
export function pointerInsertsAfter(activeRect: Rect | null | undefined, overRect: Rect | null | undefined): boolean {
  const a = centerX(activeRect);
  const o = centerX(overRect);
  return a !== null && o !== null && a > o;
}

/** Whether a resolved drop is worth persisting: a cross-dock move always is, but
 *  a within-dock reorder onto the tab's own slot is a no-op to skip. */
export function shouldApplyPlacement(
  tabsByDock: Record<DockLocation, string[]>,
  tabId: string,
  target: DropTarget,
  sourceDock: DockLocation | null,
): boolean {
  if (target.dock !== sourceDock) return true;
  const from = tabsByDock[target.dock].indexOf(tabId);
  return from >= 0 && from !== target.index;
}

/** Translate an insertion index in a dock's *visible* tab list to the index in
 *  its *full* persisted list. They differ when the dock holds a tab that is
 *  currently hidden (an unloaded plugin pane), which still occupies a persisted
 *  slot. `fullBase` is the full dock list with the dragged tab already removed.
 *  An index at or past the visible end appends to the full list. */
export function visibleToFullIndex(
  fullBase: string[],
  visibleIndex: number,
  isVisible: (id: string) => boolean,
): number {
  const visibleSlots = fullBase.map((id, index) => ({ id, index })).filter(({ id }) => isVisible(id));
  return visibleIndex >= visibleSlots.length ? fullBase.length : visibleSlots[visibleIndex]!.index;
}

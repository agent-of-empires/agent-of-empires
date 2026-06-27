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

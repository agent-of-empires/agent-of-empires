// Built-in dockable panes (the "tool windows" of the right/bottom docks).
// Plugin-contributed panes are added dynamically at render time from the
// `pane` UI slot; see the plugin slot renderers. The activity bar maps over
// this list to draw one toggle icon per pane.

import { FileDiff, SquareTerminal, type LucideIcon } from "lucide-react";

export type BuiltinPaneId = "diff" | "terminal";

/** Where a pane is docked. Right is a vertical column beside the main view;
 *  bottom is a horizontal strip below it (left is intentionally deferred). */
export type DockLocation = "right" | "bottom";

export interface PaneDescriptor {
  id: BuiltinPaneId;
  title: string;
  icon: LucideIcon;
  defaultDock: DockLocation;
}

export const BUILTIN_PANES: PaneDescriptor[] = [
  { id: "diff", title: "Diff", icon: FileDiff, defaultDock: "right" },
  { id: "terminal", title: "Terminal", icon: SquareTerminal, defaultDock: "right" },
];

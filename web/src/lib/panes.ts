// Built-in dockable panes (the "tool windows" of the right/bottom docks).
// Plugin-contributed panes are added dynamically at render time from the
// `pane` UI slot; see the plugin slot renderers. The activity bar maps over
// this list to draw one toggle icon per pane.

import { Bot, FileDiff, SquareTerminal, type LucideIcon } from "lucide-react";

export type BuiltinPaneId = "diff" | "terminal" | "agents";

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
  { id: "agents", title: "Sub agents", icon: Bot, defaultDock: "right" },
];

// Terminal panes are the one kind that supports multiple instances as tabs
// (#2437): the activity-bar key stays "terminal", but each tab has its own
// instance id "terminal:<index>" mapping to a backend tmux session at that
// index. Diff and plugin panes are single-instance, so their tab id equals
// their kind id.
export const TERMINAL_KIND = "terminal";

// Only `terminal:<digits>` is a valid terminal tab id. Strict matching keeps a
// malformed id (e.g. "terminal:1junk") from aliasing a real tmux pane index.
const TERMINAL_TAB_ID_RE = /^terminal:(\d+)$/;

export function terminalTabId(index: number): string {
  return `terminal:${index}`;
}

export function isTerminalTabId(id: string): boolean {
  return TERMINAL_TAB_ID_RE.test(id);
}

/** Backend tmux index for a "terminal:<n>" tab id; 0 for anything malformed. */
export function terminalIndexOf(id: string): number {
  const m = TERMINAL_TAB_ID_RE.exec(id);
  return m ? Number.parseInt(m[1]!, 10) : 0;
}

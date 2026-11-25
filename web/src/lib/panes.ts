// Built-in dockable panes; plugin panes come from the `pane` UI slot.

import { Bot, FileDiff, FolderTree, SquareTerminal, type LucideIcon } from "lucide-react";

export type BuiltinPaneId = "diff" | "terminal" | "agents" | "files";

export type DockLocation = "right" | "bottom";

export interface PaneDescriptor {
  id: BuiltinPaneId;
  title: string;
  icon: LucideIcon;
  defaultDock: DockLocation;
}

export const BUILTIN_PANES: PaneDescriptor[] = [
  { id: "diff", title: "Diff", icon: FileDiff, defaultDock: "right" },
  { id: "files", title: "Files", icon: FolderTree, defaultDock: "right" },
  { id: "terminal", title: "Terminal", icon: SquareTerminal, defaultDock: "bottom" },
  { id: "agents", title: "Background", icon: Bot, defaultDock: "right" },
];

// Terminals are the only multi-instance pane. Tab `terminal:<sessionId>:<n>` is bound at creation to that
// Session's paired shell at tmux index n, whichever Session is selected.
export const TERMINAL_KIND = "terminal";

/** Where a terminal tab opens when no other terminal tab is open: below the main view, as in an editor. */
export const TERMINAL_DOCK: DockLocation = "bottom";

// Strict, so a malformed id cannot alias a real tmux index.
const TERMINAL_TAB_ID_RE = /^terminal:([A-Za-z0-9_-]+):(\d+)$/;
// Before tabs were bound, `terminal:<n>` showed the selected Session's shell n.
const UNBOUND_TERMINAL_TAB_ID_RE = /^terminal:(\d+)$/;

export interface TerminalRef {
  sessionId: string;
  index: number;
}

export function terminalTabId(sessionId: string, index: number): string {
  return `terminal:${sessionId}:${index}`;
}

export function isTerminalTabId(id: string): boolean {
  return TERMINAL_TAB_ID_RE.test(id);
}

export function parseTerminalTabId(id: string): TerminalRef | null {
  const m = TERMINAL_TAB_ID_RE.exec(id);
  return m ? { sessionId: m[1]!, index: Number.parseInt(m[2]!, 10) } : null;
}

/** The `terminal:<n>` id of a layout saved before tabs were bound, rebound to the Session that layout belongs to. */
export function bindUnboundTerminalTabId(id: string, sessionId: string): string {
  const m = UNBOUND_TERMINAL_TAB_ID_RE.exec(id);
  return m ? terminalTabId(sessionId, Number.parseInt(m[1]!, 10)) : id;
}

import { useCallback } from "react";

import { dockOf, dockTabs, isActiveTab, isDockCollapsed, type DockLayout, type PaneLayoutApi } from "../lib/paneLayout";
import { isTerminalTabId, terminalIndexOf, terminalTabId, type DockLocation } from "../lib/panes";
import type { ShellMode } from "../components/PairedTerminal";
import type { RightPanelView } from "../lib/rightPanelView";
import { dispatchFocusTerminal, setPendingTerminalFocus } from "../lib/terminalFocus";

/**
 * Shows and focuses the paired shell, never toggling back to the Agent; returns the terminal it shows.
 * `prepare` runs first with that terminal's index and returns whether it will remount the shell.
 */
export type RevealPairedTerminal = (prepare?: (terminalIndex: number) => boolean) => number;

export function usePairedTerminalReveal({
  singlePane,
  layout,
  openTab,
  activateTab,
  setRightPanelView,
}: {
  singlePane: boolean;
  layout: DockLayout;
  openTab: PaneLayoutApi["openTab"];
  activateTab: PaneLayoutApi["activateTab"];
  setRightPanelView: (view: RightPanelView) => void;
}): RevealPairedTerminal {
  return useCallback(
    (prepare) => {
      if (singlePane) {
        prepare?.(0);
        // Below md there is one full-viewport pane. Promote the paired view, then
        // dispatch focus on the next frame: the inactive layer is inert until React
        // commits the switch. The paired shell mounts lazily, so its PTY may not be
        // ready when the dispatch fires; latch the intent too.
        setRightPanelView("paired");
        setPendingTerminalFocus("paired");
        requestAnimationFrame(() => dispatchFocusTerminal("paired"));
        return 0;
      }
      // The paired shell only mounts while a terminal tab is its group's active tab
      // in an open dock. Prefer a visible one, then an active one, then the first.
      const shown = (id: string) => {
        const dock = dockOf(layout, id);
        return dock !== null && isActiveTab(layout, id) && !isDockCollapsed(layout, dock);
      };
      const terminalTabs = (["right", "bottom"] as DockLocation[])
        .flatMap((d) => dockTabs(layout, d))
        .filter(isTerminalTabId);
      const termTab =
        terminalTabs.find(shown) ??
        terminalTabs.find((id) => isActiveTab(layout, id)) ??
        terminalTabs[0] ??
        terminalTabId(0);
      const index = terminalIndexOf(termTab);
      const remounts = prepare?.(index) ?? false;
      if (shown(termTab) && !remounts) {
        // Mounted and staying: focus synchronously so rapid agent/paired toggles stay deterministic.
        dispatchFocusTerminal("paired");
        return index;
      }
      // Unmounted, hidden or about to remount: latch the intent for the shell to take once
      // ready, and reveal its tab (activateTab also opens a collapsed dock).
      setPendingTerminalFocus("paired");
      const termDock = dockOf(layout, termTab);
      if (termDock) activateTab(termDock, termTab);
      else openTab(termTab, "right");
      return index;
    },
    [singlePane, layout, openTab, activateTab, setRightPanelView],
  );
}

/** Open Terminal for `sessionId`: only the active Session's, so it never switches another Session's shell. */
export function useOpenPairedTerminal(
  activeSessionId: string | null,
  reveal: RevealPairedTerminal,
  /** The mode the Session's shell shows now; a different one remounts it. */
  shownMode: (sessionId: string, terminalIndex: number) => ShellMode,
  setMode: (sessionId: string, terminalIndex: number, mode: ShellMode) => void,
): (sessionId: string, mode: ShellMode) => void {
  return useCallback(
    (sessionId: string, mode: ShellMode) => {
      if (sessionId !== activeSessionId) return;
      reveal((terminalIndex) => {
        const remounts = shownMode(sessionId, terminalIndex) !== mode;
        setMode(sessionId, terminalIndex, mode);
        return remounts;
      });
    },
    [activeSessionId, reveal, shownMode, setMode],
  );
}

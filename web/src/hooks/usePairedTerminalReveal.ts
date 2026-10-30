import { useCallback } from "react";

import { dockOf, dockTabs, isActiveTab, type DockLayout, type PaneLayoutApi } from "../lib/paneLayout";
import { isTerminalTabId, terminalIndexOf, terminalTabId, type DockLocation } from "../lib/panes";
import type { ShellMode } from "../components/PairedTerminal";
import type { RightPanelView } from "../lib/rightPanelView";
import { dispatchFocusTerminal, setPendingTerminalFocus } from "../lib/terminalFocus";

/** Shows and focuses the paired shell, never toggling back to the Agent; returns the terminal it shows. */
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
}): () => number {
  return useCallback((): number => {
    if (singlePane) {
      // Below md there is one full-viewport pane. Promote the paired view, then
      // dispatch focus on the next frame: the inactive layer is inert until React
      // commits the switch. The paired shell mounts lazily, so its PTY may not be
      // ready when the dispatch fires; latch the intent too.
      setRightPanelView("paired");
      setPendingTerminalFocus("paired");
      requestAnimationFrame(() => dispatchFocusTerminal("paired"));
      return 0;
    }
    // The paired shell only mounts while a terminal tab is its group's active tab.
    // Prefer one already active over the first, so a multi-group layout focuses
    // the live terminal instead of switching another group's tab.
    const terminalTabs = (["right", "bottom"] as DockLocation[])
      .flatMap((d) => dockTabs(layout, d))
      .filter(isTerminalTabId);
    const termTab = terminalTabs.find((id) => isActiveTab(layout, id)) ?? terminalTabs[0] ?? terminalTabId(0);
    const termDock = dockOf(layout, termTab);
    if (termDock && isActiveTab(layout, termTab)) {
      // Mounted: focus synchronously so rapid agent/paired toggles stay deterministic.
      dispatchFocusTerminal("paired");
      return terminalIndexOf(termTab);
    }
    // Not mounted: latch the intent and open its tab; the shell grabs focus once its PTY is ready.
    setPendingTerminalFocus("paired");
    if (termDock) activateTab(termDock, termTab);
    else openTab(termTab, "right");
    return terminalIndexOf(termTab);
  }, [singlePane, layout, openTab, activateTab, setRightPanelView]);
}

/** Open Terminal for `sessionId`: only the active Session's, so it never switches another Session's shell. */
export function useOpenPairedTerminal(
  activeSessionId: string | null,
  reveal: () => number,
  setMode: (sessionId: string, terminalIndex: number, mode: ShellMode) => void,
): (sessionId: string, mode: ShellMode) => void {
  return useCallback(
    (sessionId: string, mode: ShellMode) => {
      if (sessionId !== activeSessionId) return;
      setMode(sessionId, reveal(), mode);
    },
    [activeSessionId, reveal, setMode],
  );
}

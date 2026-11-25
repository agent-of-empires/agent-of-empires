import { useCallback } from "react";

import { confirmFileEditPanes } from "../lib/fileEditGuard";
import {
  dockOf,
  isActiveTab,
  isDockCollapsed,
  sessionTerminals,
  terminalPlacements,
  type DockLayout,
  type PaneLayoutApi,
} from "../lib/paneLayout";
import { parseTerminalTabId, terminalTabId, TERMINAL_DOCK, type TerminalRef } from "../lib/panes";
import type { ShellMode } from "../components/PairedTerminal";
import { dispatchFocusTerminal, setPendingTerminalFocus } from "../lib/terminalFocus";

/**
 * Shows and focuses the active Session's paired shell, never toggling back to the Agent; returns the terminal it
 * shows, or null when the user keeps an unsaved file edit. `prepare` runs first with that terminal's index and
 * returns whether it will remount the shell.
 */
export type RevealPairedTerminal = (prepare?: (terminalIndex: number) => boolean) => number | null;

/** Shows and focuses one Session's shell, opening a tab for it if none is; `prepare` as for {@link RevealPairedTerminal}. */
export type RevealShell = (shell: TerminalRef, prepare?: () => boolean) => boolean;

interface Args {
  singlePane: boolean;
  activeSessionId: string | null;
  layout: DockLayout;
  openTab: PaneLayoutApi["openTab"];
  activateTab: PaneLayoutApi["activateTab"];
  /** Promotes the mobile paired view to show `shell`. */
  showMobileShell: (shell: TerminalRef) => void;
}

export function usePairedTerminalReveal({
  singlePane,
  activeSessionId,
  layout,
  openTab,
  activateTab,
  showMobileShell,
}: Args): { revealActive: RevealPairedTerminal; revealShell: RevealShell } {
  const shown = useCallback(
    (id: string) => {
      const dock = dockOf(layout, id);
      return dock !== null && isActiveTab(layout, id) && !isDockCollapsed(layout, dock);
    },
    [layout],
  );

  const reveal = useCallback(
    (tabId: string, shell: TerminalRef, remounts: boolean) => {
      if (singlePane) {
        // Below md there is one full-viewport pane. Promote the paired view, then
        // dispatch focus on the next frame: the inactive layer is inert until React
        // commits the switch. The paired shell mounts lazily, so its PTY may not be
        // ready when the dispatch fires; latch the intent too.
        showMobileShell(shell);
        setPendingTerminalFocus("paired", shell.index, shell.sessionId);
        requestAnimationFrame(() => dispatchFocusTerminal("paired", shell.index, shell.sessionId));
        return;
      }
      if (shown(tabId) && !remounts) {
        // Mounted and staying: focus synchronously so rapid agent/paired toggles stay deterministic.
        dispatchFocusTerminal("paired", shell.index, shell.sessionId);
        return;
      }
      // Unmounted, hidden or about to remount: latch the intent for the shell to take once
      // ready, and reveal its tab (activateTab also opens a collapsed dock). A new tab joins the other terminals.
      setPendingTerminalFocus("paired", shell.index, shell.sessionId);
      const termDock = dockOf(layout, tabId);
      if (termDock) activateTab(termDock, tabId);
      else openTab(tabId, terminalPlacements(layout)[0]?.dock ?? TERMINAL_DOCK);
    },
    [singlePane, shown, layout, openTab, activateTab, showMobileShell],
  );

  const revealActive = useCallback<RevealPairedTerminal>(
    (prepare) => {
      // The mobile paired view replaces the main view, which may hold an unsaved edit.
      if (!activeSessionId || (singlePane && !confirmFileEditPanes([null]))) return null;
      // The mobile view shows the primary shell; a dock prefers a visible tab, then an active one, then the first.
      const tabs = singlePane ? [] : sessionTerminals(layout, activeSessionId).map((t) => t.id);
      const tabId =
        tabs.find(shown) ?? tabs.find((id) => isActiveTab(layout, id)) ?? tabs[0] ?? terminalTabId(activeSessionId, 0);
      const shell = parseTerminalTabId(tabId)!;
      reveal(tabId, shell, prepare?.(shell.index) ?? false);
      return shell.index;
    },
    [activeSessionId, singlePane, layout, shown, reveal],
  );

  const revealShell = useCallback<RevealShell>(
    (shell, prepare) => {
      if (singlePane && !confirmFileEditPanes([null])) return false;
      reveal(terminalTabId(shell.sessionId, shell.index), shell, prepare?.() ?? false);
      return true;
    },
    [singlePane, reveal],
  );

  return { revealActive, revealShell };
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

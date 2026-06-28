/* eslint-disable react-refresh/only-export-components */
// Shares the plugin UI-state snapshot (#2366) across the dashboard. The host
// renders nothing; it ships the slot entries and the daemon's worker-pushed
// state, and these components draw them. One poll lives in the provider so
// TopBar, the sidebar rows, the dashboard cards, and the right panel all read
// the same snapshot without each opening its own clock.

import { createContext, useContext, type ReactNode } from "react";
import { usePluginUiState } from "../hooks/usePluginUiState";
import type { PluginUiEntry } from "./api";

// Entries, revisions, the refresh flag, and poke live in separate contexts: the
// flag and revisions toggle on polls, and folding them into the entries context
// would re-render all entry consumers (rows, badges, cards, top bar) on each
// change even though they never read them.
const PluginUiEntriesContext = createContext<PluginUiEntry[]>([]);
const PluginUiRefreshingContext = createContext(false);
const PluginUiRevisionsContext = createContext<Record<string, Record<string, number>>>({});
const PluginUiPokeContext = createContext<() => void>(() => {});

export function PluginUiProvider({ children }: { children: ReactNode }) {
  const { entries, revisions, isRefreshing, poke } = usePluginUiState();
  return (
    <PluginUiEntriesContext.Provider value={entries}>
      <PluginUiRevisionsContext.Provider value={revisions}>
        <PluginUiPokeContext.Provider value={poke}>
          <PluginUiRefreshingContext.Provider value={isRefreshing}>{children}</PluginUiRefreshingContext.Provider>
        </PluginUiPokeContext.Provider>
      </PluginUiRevisionsContext.Provider>
    </PluginUiEntriesContext.Provider>
  );
}

/** All current plugin UI entries. Filter with the selectors in `pluginUi.ts`. */
export function usePluginUiEntries(): PluginUiEntry[] {
  return useContext(PluginUiEntriesContext);
}

/** True while a background ui-state poll has been in flight past the indicator
 *  delay. Lets a pane renderer show a refresh-in-progress affordance. */
export function usePluginUiRefreshing(): boolean {
  return useContext(PluginUiRefreshingContext);
}

/** The UI mutation counter for one plugin pane's scope (a session id, or the
 *  global `""` scope), 0 if none seen yet. A pane action holds its spinner until
 *  this moves off the baseline the action POST returned, so a different
 *  session's push for the same plugin cannot clear it. */
export function usePluginUiRevision(pluginId: string, sessionId?: string): number {
  return useContext(PluginUiRevisionsContext)[pluginId]?.[sessionId ?? ""] ?? 0;
}

/** Run a ui-state poll now and briefly boost the cadence, so a just-fired pane
 *  action's result (and its revision bump) shows up without waiting a full tick. */
export function usePluginUiPoke(): () => void {
  return useContext(PluginUiPokeContext);
}

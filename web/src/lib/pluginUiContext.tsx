/* eslint-disable react-refresh/only-export-components */
// Shares the plugin UI-state snapshot (#2366) across the dashboard. The host
// renders nothing; it ships the slot entries and the daemon's worker-pushed
// state, and these components draw them. One poll lives in the provider so
// TopBar, the sidebar rows, the dashboard cards, and the right panel all read
// the same snapshot without each opening its own clock.

import { createContext, useContext, type ReactNode } from "react";
import { usePluginUiState } from "../hooks/usePluginUiState";
import type { PluginUiEntry } from "./api";

// Entries and the refresh flag live in separate contexts: the flag toggles on
// every slow poll, and folding it into the entries context would re-render all
// entry consumers (rows, badges, cards, top bar) on each toggle even though
// they never read it.
const PluginUiEntriesContext = createContext<PluginUiEntry[]>([]);
const PluginUiRefreshingContext = createContext(false);

export function PluginUiProvider({ children }: { children: ReactNode }) {
  const { entries, isRefreshing } = usePluginUiState();
  return (
    <PluginUiEntriesContext.Provider value={entries}>
      <PluginUiRefreshingContext.Provider value={isRefreshing}>{children}</PluginUiRefreshingContext.Provider>
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

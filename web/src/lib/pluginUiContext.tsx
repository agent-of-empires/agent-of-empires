/* eslint-disable react-refresh/only-export-components */
// One plugin UI-state poll shared by the whole dashboard.

import { createContext, useContext, type ReactNode } from "react";
import { usePluginUiState } from "../hooks/usePluginUiState";
import type { PluginUiEntry } from "./api";

// Separate contexts so poll-driven flag and revision changes don't re-render every entry consumer.
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

/** Filter with the selectors in `pluginUi.ts`. */
export function usePluginUiEntries(): PluginUiEntry[] {
  return useContext(PluginUiEntriesContext);
}

/** True while a background poll runs past the indicator delay. */
export function usePluginUiRefreshing(): boolean {
  return useContext(PluginUiRefreshingContext);
}

/** Per-scope mutation counter; a pane action holds its spinner until it moves off the returned baseline. */
export function usePluginUiRevision(pluginId: string, sessionId?: string): number {
  return useContext(PluginUiRevisionsContext)[pluginId]?.[sessionId ?? ""] ?? 0;
}

/** Poll now and briefly boost the cadence so an action's result shows promptly. */
export function usePluginUiPoke(): () => void {
  return useContext(PluginUiPokeContext);
}

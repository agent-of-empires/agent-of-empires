/* eslint-disable react-refresh/only-export-components */
// One plugin UI-state poll shared by the whole dashboard.

import { createContext, useContext, useMemo, useState, type ReactNode } from "react";
import { usePluginUiState } from "../hooks/usePluginUiState";
import type { PluginPage, PluginUiEntry } from "./api";
import { pluginAttentionSessionIds } from "./pluginUi";

// Separate contexts so poll-driven flag and revision changes don't re-render every entry consumer.
const PluginUiEntriesContext = createContext<PluginUiEntry[]>([]);
const PluginUiAttentionContext = createContext<ReadonlySet<string>>(new Set());
const PluginUiRefreshingContext = createContext(false);
const PluginUiRevisionsContext = createContext<Record<string, Record<string, number>>>({});
const PluginUiPokeContext = createContext<() => void>(() => {});
const PluginUiPagesContext = createContext<PluginPage[] | null>(null);

/** Every poll delivers fresh entries, so keep the previous set while its members are unchanged to spare dependents the recompute. */
function useStableAttention(entries: PluginUiEntry[]): ReadonlySet<string> {
  const next = useMemo(() => pluginAttentionSessionIds(entries), [entries]);
  const [stable, setStable] = useState(next);
  const same = next.size === stable.size && [...next].every((id) => stable.has(id));
  if (!same) setStable(next);
  return same ? stable : next;
}

export function PluginUiProvider({ children }: { children: ReactNode }) {
  const { entries, revisions, pages, isRefreshing, poke } = usePluginUiState();
  const attention = useStableAttention(entries);
  return (
    <PluginUiEntriesContext.Provider value={entries}>
      <PluginUiAttentionContext.Provider value={attention}>
        <PluginUiRevisionsContext.Provider value={revisions}>
          <PluginUiPokeContext.Provider value={poke}>
            <PluginUiRefreshingContext.Provider value={isRefreshing}>
              <PluginUiPagesContext.Provider value={pages}>{children}</PluginUiPagesContext.Provider>
            </PluginUiRefreshingContext.Provider>
          </PluginUiPokeContext.Provider>
        </PluginUiRevisionsContext.Provider>
      </PluginUiAttentionContext.Provider>
    </PluginUiEntriesContext.Provider>
  );
}

/** Filter with the selectors in `pluginUi.ts`. */
export function usePluginUiEntries(): PluginUiEntry[] {
  return useContext(PluginUiEntriesContext);
}

/** Session ids with a plugin danger/warn `row-column` tone; keeps its identity across polls while the membership is unchanged. */
export function usePluginAttention(): ReadonlySet<string> {
  return useContext(PluginUiAttentionContext);
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

/** Pages active plugins declare, `null` before the first poll; keeps its identity across polls while unchanged. */
export function usePluginPages(): PluginPage[] | null {
  return useContext(PluginUiPagesContext);
}

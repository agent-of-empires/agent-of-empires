/* eslint-disable react-refresh/only-export-components */
// Shares the plugin UI-state snapshot (#2366) across the dashboard. The host
// renders nothing; it ships the slot entries and the daemon's worker-pushed
// state, and these components draw them. One poll lives in the provider so
// TopBar, the sidebar rows, the dashboard cards, and the right panel all read
// the same snapshot without each opening its own clock.

import { createContext, useContext, type ReactNode } from "react";
import { usePluginUiState } from "../hooks/usePluginUiState";
import type { PluginUiEntry } from "./api";

const PluginUiContext = createContext<PluginUiEntry[]>([]);

export function PluginUiProvider({ children }: { children: ReactNode }) {
  const entries = usePluginUiState();
  return <PluginUiContext.Provider value={entries}>{children}</PluginUiContext.Provider>;
}

/** All current plugin UI entries. Filter with the selectors in `pluginUi.ts`. */
export function usePluginUiEntries(): PluginUiEntry[] {
  return useContext(PluginUiContext);
}

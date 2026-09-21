/* eslint-disable react-refresh/only-export-components */
// Structured view display preferences from the daemon's resolved `[acp]` config, republished from `/api/about` so deep tool cards need no prop drilling.

import { createContext, useContext, type ReactNode } from "react";

export interface AcpPrefs {
  showToolDurations: boolean;
  /** Transcript retention cap; 0 means unlimited. */
  replayEvents: number;
  compactionReminder: boolean;
  compactionReminderPercent: number;
}

const DEFAULT_PREFS: AcpPrefs = {
  showToolDurations: true,
  replayEvents: 0,
  compactionReminder: false,
  compactionReminderPercent: 75,
};

const AcpPrefsContext = createContext<AcpPrefs>(DEFAULT_PREFS);

export function AcpPrefsProvider({ value, children }: { value: AcpPrefs; children: ReactNode }) {
  return <AcpPrefsContext.Provider value={value}>{children}</AcpPrefsContext.Provider>;
}

export function useAcpPrefs(): AcpPrefs {
  return useContext(AcpPrefsContext);
}

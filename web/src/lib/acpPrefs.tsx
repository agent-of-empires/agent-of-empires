/* eslint-disable react-refresh/only-export-components */
// Structured view display preferences sourced from the daemon's resolved
// `[acp]` config (config.toml). Single source of truth: the
// `/api/about` endpoint exposes the resolved active-profile values as
// `ServerAbout.acp_*`. App.tsx fetches that on mount and
// republishes the relevant slice through this context so any structured view
// renderer (deeply-nested tool cards in particular) can subscribe
// without prop-drilling.
//
// Cross-device by construction: every browser pointed at the same
// daemon reads the same value. Toggling from the web Settings panel
// rewrites config.toml via `PATCH /api/profiles/:name/settings`, then
// `App.refreshServerAbout()` re-fetches `/api/about` and the context
// repopulates.

import { createContext, useContext, type ReactNode } from "react";

export interface AcpPrefs {
  /** Resolved `acp.show_tool_durations` from the active profile.
   *  When true, tool-card headers display a per-call elapsed-time
   *  label. Imprecise on claude-agent-acp today; see
   *  `CardChromeProps.startedAt` in ToolCards.tsx for the upstream
   *  limitation. */
  showToolDurations: boolean;
  /** Resolved `acp.replay_events` from the active profile. Cap
   *  on the in-memory activity buffer the reducer holds (so the
   *  rendered transcript matches the user's chosen retention).
   *  0 means unlimited. See #1111. */
  replayEvents: number;
}

const DEFAULT_PREFS: AcpPrefs = {
  showToolDurations: true,
  replayEvents: 0,
};

const AcpPrefsContext = createContext<AcpPrefs>(DEFAULT_PREFS);

export function AcpPrefsProvider({ value, children }: { value: AcpPrefs; children: ReactNode }) {
  return <AcpPrefsContext.Provider value={value}>{children}</AcpPrefsContext.Provider>;
}

export function useAcpPrefs(): AcpPrefs {
  return useContext(AcpPrefsContext);
}

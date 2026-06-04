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
  /** Resolved `acp.queue_drain_mode` from the active profile. Selects
   *  how the composer drains client-side queued follow-up prompts on
   *  Stopped: `combined` (default) joins them with blank lines into a
   *  single prompt; `serial` fires one entry at a time. See #1031. */
  queueDrainMode: "combined" | "serial";
  /** Resolved `acp.force_end_turn_threshold_secs` from the active
   *  profile. Seconds of streaming inactivity after which the structured view
   *  spinner offers a "Force end turn" escape hatch. See #1100. */
  forceEndTurnThresholdSecs: number;
  /** Resolved `acp.replay_events` from the active profile. Cap
   *  on the in-memory activity buffer the reducer holds (so the
   *  rendered transcript matches the user's chosen retention).
   *  0 means unlimited. See #1111. */
  replayEvents: number;
}

const DEFAULT_PREFS: AcpPrefs = {
  showToolDurations: true,
  queueDrainMode: "combined",
  forceEndTurnThresholdSecs: 30,
  replayEvents: 0,
};

const AcpPrefsContext = createContext<AcpPrefs>(DEFAULT_PREFS);

export function AcpPrefsProvider({
  value,
  children,
}: {
  value: AcpPrefs;
  children: ReactNode;
}) {
  return (
    <AcpPrefsContext.Provider value={value}>
      {children}
    </AcpPrefsContext.Provider>
  );
}

export function useAcpPrefs(): AcpPrefs {
  return useContext(AcpPrefsContext);
}

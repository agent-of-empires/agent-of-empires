import { createContext, useContext } from "react";

import { IDLE_DECAY_WINDOW_MS } from "./session";

const DEFAULT_IDLE_DECAY_WINDOW_MS = IDLE_DECAY_WINDOW_MS;

export const IdleDecayWindowContext = createContext<number>(DEFAULT_IDLE_DECAY_WINDOW_MS);

/** Read the configured idle decay window from `/api/settings` payloads.
 * Missing, invalid, or negative values fall back to the dashboard default
 * (off). The server stores minutes; the web helpers consume milliseconds. */
export function parseIdleDecayWindowMs(settings: Record<string, unknown> | null | undefined): number {
  const theme = settings?.theme;
  if (!theme || typeof theme !== "object") {
    return DEFAULT_IDLE_DECAY_WINDOW_MS;
  }

  const minutes = (theme as Record<string, unknown>).idle_decay_minutes;
  if (typeof minutes !== "number" || !Number.isFinite(minutes) || minutes <= 0) {
    return DEFAULT_IDLE_DECAY_WINDOW_MS;
  }

  return minutes * 60 * 1000;
}

export function useIdleDecayWindowMs(): number {
  return useContext(IdleDecayWindowContext);
}

import { createContext, useContext } from "react";

/** Default to on, matching the server's `session.show_session_colors` default
 *  and the value the gate falls back to before `/api/settings` has resolved. */
const DEFAULT_SESSION_COLORS_ENABLED = true;

export const SessionColorsContext = createContext<boolean>(DEFAULT_SESSION_COLORS_ENABLED);

/** Read `session.show_session_colors` from an `/api/settings` payload. Only an
 *  explicit `false` disables it; a missing or malformed value keeps the default
 *  (on), so an older daemon that doesn't send the field still shows colors. */
export function parseSessionColorsEnabled(settings: Record<string, unknown> | null | undefined): boolean {
  const session = settings?.session;
  if (!session || typeof session !== "object") {
    return DEFAULT_SESSION_COLORS_ENABLED;
  }
  return (session as Record<string, unknown>).show_session_colors !== false;
}

export function useSessionColorsEnabled(): boolean {
  return useContext(SessionColorsContext);
}

import { createContext, useContext } from "react";

/** Default to on, matching the server's `session.unread_indicator` default and
 *  the value the gate falls back to before `/api/settings` has resolved. */
const DEFAULT_UNREAD_INDICATOR_ENABLED = true;

export const UnreadIndicatorContext = createContext<boolean>(DEFAULT_UNREAD_INDICATOR_ENABLED);

/** Read `session.unread_indicator` from an `/api/settings` payload. Only an
 *  explicit `false` disables it; a missing or malformed value keeps the
 *  default (on), so an older daemon that doesn't send the field still shows
 *  the indicator. */
export function parseUnreadIndicatorEnabled(settings: Record<string, unknown> | null | undefined): boolean {
  const session = settings?.session;
  if (!session || typeof session !== "object") {
    return DEFAULT_UNREAD_INDICATOR_ENABLED;
  }
  return (session as Record<string, unknown>).unread_indicator !== false;
}

export function useUnreadIndicatorEnabled(): boolean {
  return useContext(UnreadIndicatorContext);
}

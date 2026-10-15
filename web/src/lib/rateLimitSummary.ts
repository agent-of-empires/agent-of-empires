// Sidebar rate-limit indicator from the polled session payload.

import type { SessionResponse } from "./types";

export interface SidebarRateLimit {
  count: number;
  /** Soonest `resets_at`, or null. */
  resetsAt: string | null;
}

/** Null when no session is rate-limited; one without a reported reset still counts. */
export function summarizeRateLimits(sessions: readonly Pick<SessionResponse, "rate_limit">[]): SidebarRateLimit | null {
  let count = 0;
  let soonest: string | null = null;
  for (const session of sessions) {
    const info = session.rate_limit;
    if (!info) continue;
    count += 1;
    if (info.resets_at !== null && (soonest === null || Date.parse(info.resets_at) < Date.parse(soonest))) {
      soonest = info.resets_at;
    }
  }
  return count === 0 ? null : { count, resetsAt: soonest };
}

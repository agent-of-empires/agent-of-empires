// Sidebar "rate-limited" indicator, computed from the session payload the
// sidebar already polls. The daemon reports `rate_limit` from its durable
// event-store park, so a session that resumed overnight with no tab open
// clears here on the next poll; the old localStorage mirror only updated
// while a structured view hook was mounted (#3514).

import type { SessionResponse } from "./types";

export interface SidebarRateLimit {
  /** How many of the given sessions are currently rate-limited. */
  count: number;
  /** Soonest `resets_at` across the rate-limited sessions, or null. */
  resetsAt: string | null;
}

/** Aggregate for one workspace row, or null when none of its sessions is
 *  rate-limited. A session whose agent reported no reset still counts; it
 *  just contributes no time to the "resets at" hint (#3152). */
export function summarizeRateLimits(sessions: readonly Pick<SessionResponse, "rate_limit">[]): SidebarRateLimit | null {
  let count = 0;
  let soonest: string | null = null;
  for (const session of sessions) {
    const info = session.rate_limit;
    if (!info) continue;
    count += 1;
    if (info.resets_at !== null && (soonest === null || info.resets_at < soonest)) {
      soonest = info.resets_at;
    }
  }
  return count === 0 ? null : { count, resetsAt: soonest };
}

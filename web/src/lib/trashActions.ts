// Trash / restore action loops (#2489), extracted from App so the
// per-session apply + aggregate toast logic is unit-testable rather than
// reachable only through the structured-view bundle.

import { restoreSession, trashSession } from "./api";
import type { SessionResponse } from "./types";

/** A toast sink; both methods are optional so callers can pass the bus
 *  handler before it is wired without a guard. */
export interface Notifier {
  error?: (message: string) => void;
  info?: (message: string) => void;
}

interface TrashDeps {
  /** Re-bucket a session from the trash/restore response without waiting for
   *  the next poll. */
  applySession: (session: SessionResponse) => void;
  notify: Notifier | null;
}

/** Trash every id, applying each returned snapshot. On a failed id, calls
 *  `onError(id)` so the caller can flag the row. Returns true iff all
 *  succeeded; toasts the aggregate result. */
export async function trashSessions(
  ids: string[],
  deps: TrashDeps & { onError: (id: string) => void },
): Promise<boolean> {
  let anyFailed = false;
  for (const id of ids) {
    const res = await trashSession(id);
    if (res) {
      deps.applySession(res);
    } else {
      anyFailed = true;
      deps.onError(id);
    }
  }
  if (anyFailed) {
    deps.notify?.error?.("Failed to move session to trash");
  } else {
    deps.notify?.info?.("Moved to trash");
  }
  return !anyFailed;
}

/** Restore every id, applying each returned snapshot. Returns true iff all
 *  succeeded; toasts the aggregate result. */
export async function restoreSessions(ids: string[], deps: TrashDeps): Promise<boolean> {
  let anyFailed = false;
  for (const id of ids) {
    const res = await restoreSession(id);
    if (res) {
      deps.applySession(res);
    } else {
      anyFailed = true;
    }
  }
  if (anyFailed) {
    deps.notify?.error?.("Failed to restore session");
  } else {
    deps.notify?.info?.("Session restored");
  }
  return !anyFailed;
}

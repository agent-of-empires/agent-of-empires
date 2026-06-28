// Trash / restore action loops (#2489), extracted from App so the
// per-session apply + aggregate toast logic is unit-testable rather than
// reachable only through the structured-view bundle.

import { deleteSession, restoreSession, trashSession } from "./api";
import type { DeleteSessionOptions } from "./api";
import type { SessionResponse, SessionStatus } from "./types";

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

interface DeleteWorkspaceDeps {
  /** Reflect a per-session lifecycle status optimistically (Deleting / Error). */
  setStatus: (id: string, status: SessionStatus) => void;
  /** Drop a deleted session's local-only state (acp cache, draft, comments).
   *  Run only after the server delete for that id succeeds. */
  purgeLocal: (id: string) => void;
  /** Navigate away from the deleted session (to the dashboard root). */
  navigateHome: () => void;
  notify: Notifier | null;
}

/** Permanently delete every session in a workspace (#2530). All sessions share
 *  one git worktree and branch, so the caller's worktree/branch cleanup options
 *  run exactly once, on the primary (`sessions[0]`); a failed primary aborts
 *  before any sibling record is destroyed (the shared worktree is still
 *  present, so removing siblings would orphan the workspace), and siblings
 *  never re-run the non-idempotent worktree removal. Redirects home only once
 *  the currently-open session has actually been deleted, not merely because it
 *  belonged to the workspace (#2539). Local cleanup runs per id only after that
 *  id's delete succeeds, so a failure never strands a draft or cache. */
export async function deleteWorkspaceSessions(
  sessions: SessionResponse[],
  options: DeleteSessionOptions,
  activeSessionId: string | null,
  deps: DeleteWorkspaceDeps,
): Promise<void> {
  const primary = sessions[0];
  if (!primary) return;
  const ids = sessions.map((s) => s.id);
  const activeWorkspaceSessionId = activeSessionId != null && ids.includes(activeSessionId) ? activeSessionId : null;

  for (const id of ids) deps.setStatus(id, "Deleting");

  let activeDeleted = false;
  const primaryResult = await deleteSession(primary.id, options);
  if (!primaryResult.ok) {
    for (const id of ids) deps.setStatus(id, "Error");
    deps.notify?.error?.(primaryResult.error || "Failed to delete session");
    return;
  }
  if (activeWorkspaceSessionId === primary.id) activeDeleted = true;
  deps.purgeLocal(primary.id);

  const siblingOptions: DeleteSessionOptions = { ...options, delete_worktree: false, delete_branch: false };
  let anyFailed = false;
  for (const sibling of sessions.slice(1)) {
    const res = await deleteSession(sibling.id, siblingOptions);
    if (res.ok) {
      if (activeWorkspaceSessionId === sibling.id) activeDeleted = true;
      deps.purgeLocal(sibling.id);
    } else {
      anyFailed = true;
      deps.setStatus(sibling.id, "Error");
    }
  }

  if (activeDeleted) deps.navigateHome();

  if (anyFailed) {
    deps.notify?.error?.("Some sessions could not be deleted");
    return;
  }
  // `messages` carries any user-facing note from `perform_deletion` (e.g. a
  // kept scratch path); surface the first.
  deps.notify?.info?.(primaryResult.messages?.[0] ?? (ids.length > 1 ? "Sessions deleted" : "Session deleted"));
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

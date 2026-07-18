// Trash / restore action loops (#2489), extracted from App so the
// per-session apply + aggregate toast logic is unit-testable rather than
// reachable only through the structured-view bundle.

import { deleteWorkspace, restoreSession, trashSession } from "./api";
import type { DeleteSessionOptions } from "./api";
import type { SessionResponse, SessionStatus, Workspace } from "./types";

/** Resolve the session ids to restore when the trashed-banner Restore button
 *  fires for `sessionId`. Restore is a whole-workspace action (a workspace
 *  only lands in Trash when all its sessions are), so this returns every
 *  session id in the workspace containing `sessionId`, matching the sidebar
 *  Trash action. Falls back to just `[sessionId]` when no workspace groups it.
 *  See #2593. */
export function trashedWorkspaceRestoreIds(workspaces: Workspace[], sessionId: string): string[] {
  const ws = workspaces.find((w) => w.sessions.some((s) => s.id === sessionId));
  return ws ? ws.sessions.map((s) => s.id) : [sessionId];
}

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

/** Permanently delete every session in a workspace via the atomic backend
 *  endpoint (#2536). All sessions share one git worktree and branch; the server
 *  removes the shared worktree/branch exactly once (on `sessions[0]`, torn down
 *  last) and record-only-deletes the rest, so a mid-delete disconnect can no
 *  longer strand a record against an already-removed worktree. Redirects home
 *  only once the currently-open session was actually deleted, not merely because
 *  it belonged to the workspace (#2539). Local cleanup runs per id only after
 *  the server confirms that id was deleted, so a failure never strands a draft
 *  or cache. */
export async function deleteWorkspaceSessions(
  sessions: SessionResponse[],
  options: DeleteSessionOptions,
  activeSessionId: string | null,
  deps: DeleteWorkspaceDeps,
): Promise<void> {
  if (sessions.length === 0) return;
  const ids = sessions.map((s) => s.id);
  const activeInWorkspace = activeSessionId != null && ids.includes(activeSessionId);

  for (const id of ids) deps.setStatus(id, "Deleting");

  const result = await deleteWorkspace(ids, options);
  if (!result.ok) {
    for (const id of ids) deps.setStatus(id, "Error");
    deps.notify?.error?.(result.error || "Failed to delete session");
    return;
  }

  // The server reports exactly which ids it removed and which failed. Purge
  // local state only for confirmed-deleted ids; flag only explicitly-failed
  // ids as Error. An id in neither set (e.g. a concurrent restore kept the
  // row) is left untouched for the next poll to reconcile.
  const deleted = new Set(result.deleted ?? []);
  const failed = new Set((result.failed ?? []).map((f) => f.id));
  for (const id of ids) {
    if (deleted.has(id)) {
      deps.purgeLocal(id);
    } else if (failed.has(id)) {
      deps.setStatus(id, "Error");
    }
  }

  if (activeInWorkspace && deleted.has(activeSessionId!)) deps.navigateHome();

  if (result.failed && result.failed.length > 0) {
    deps.notify?.error?.("Some sessions could not be deleted");
    return;
  }
  // `messages` carries any user-facing note from `perform_deletion` (e.g. a
  // kept scratch path); surface the first.
  deps.notify?.info?.(result.messages?.[0] ?? (ids.length > 1 ? "Sessions deleted" : "Session deleted"));
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

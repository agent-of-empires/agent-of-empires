// Trash, restore, and delete loops with aggregate toasts, extracted from App for unit testing.

import { deleteWorkspace, restoreSession, trashSession } from "./api";
import type { DeleteSessionOptions } from "./api";
import type { SessionResponse, SessionStatus, Workspace } from "./types";

/** A workspace is only trashed as a whole, so restore every session of the workspace containing `sessionId`. */
export function trashedWorkspaceRestoreIds(workspaces: Workspace[], sessionId: string): string[] {
  const ws = workspaces.find((w) => w.sessions.some((s) => s.id === sessionId));
  return ws ? ws.sessions.map((s) => s.id) : [sessionId];
}

export interface Notifier {
  error?: (message: string) => void;
  info?: (message: string) => void;
}

/** Cleanup flags are on when any session in the workspace opts in; shared by the delete dialog and Empty Trash. */
export function workspaceCleanupDefaults(sessions: SessionResponse[]): {
  delete_worktree: boolean;
  delete_branch: boolean;
  delete_sandbox: boolean;
} {
  return {
    delete_worktree: sessions.some((s) => (s.has_cleanable_worktree ?? false) && s.cleanup_defaults.delete_worktree),
    delete_branch: sessions.some((s) => (s.has_cleanable_worktree ?? false) && s.cleanup_defaults.delete_branch),
    delete_sandbox: sessions.some((s) => s.is_sandboxed && s.cleanup_defaults.delete_sandbox),
  };
}

/** Sessions outside `selected` working in a worktree `selected` would clean up. The server keeps that worktree and its branch. */
export function sessionsSharingWorktree(selected: SessionResponse[], all: SessionResponse[]): SessionResponse[] {
  const trim = (path: string) => path.replace(/\/+$/, "");
  const ids = new Set(selected.map((s) => s.id));
  const roots = selected.filter((s) => s.has_cleanable_worktree).map((s) => trim(s.project_path));
  return all.filter((s) => {
    const path = trim(s.project_path);
    return !ids.has(s.id) && roots.some((root) => path === root || path.startsWith(`${root}/`));
  });
}

interface TrashDeps {
  applySession: (session: SessionResponse) => void;
  notify: Notifier | null;
}

/** On a failed id, calls `onError(id)`. Returns true iff all succeeded. */
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
  setStatus: (id: string, status: SessionStatus) => void;
  /** Runs only after the server confirms the delete. */
  purgeLocal: (id: string) => void;
  navigateHome: () => void;
  notify: Notifier | null;
}

/** Atomic delete of `sessions`; the server keeps a worktree any other session still uses. Local cleanup and the redirect home only follow confirmed deletions. */
export async function deleteWorkspaceSessions(
  sessions: SessionResponse[],
  options: DeleteSessionOptions,
  activeSessionId: string | null,
  deps: DeleteWorkspaceDeps,
): Promise<void> {
  if (sessions.length === 0) return;
  const ids = sessions.map((s) => s.id);
  const activeInWorkspace = activeSessionId != null && ids.includes(activeSessionId);

  // Remember what each row showed before the optimistic overlay: the server
  // restores the prior status on a 409 and on a retryable partial failure, so a
  // hardcoded replacement would be wrong for every row but `Stopped`.
  const priorStatus: Record<string, SessionStatus> = {};
  for (const session of sessions) priorStatus[session.id] = session.status;
  for (const id of ids) deps.setStatus(id, "Deleting");

  const result = await deleteWorkspace(ids, options);
  if (!result.ok) {
    // A pending teardown kept every row: the server restored their status, so
    // they are not failures and the user can retry the same delete.
    if (result.pending) {
      for (const id of ids) deps.setStatus(id, priorStatus[id] ?? "Stopped");
      deps.notify?.error?.(result.error || "Sessions are still shutting down, try again in a moment");
      return;
    }
    for (const id of ids) deps.setStatus(id, "Error");
    deps.notify?.error?.(result.error || "Failed to delete session");
    return;
  }

  // Ids in neither set (e.g. restored concurrently) are left for the next poll.
  const deleted = new Set(result.deleted ?? []);
  const failed = new Set((result.failed ?? []).map((f) => f.id));
  const retryable = new Set((result.failed ?? []).filter((f) => f.retryable).map((f) => f.id));
  for (const id of ids) {
    if (deleted.has(id)) {
      deps.purgeLocal(id);
    } else if (retryable.has(id)) {
      deps.setStatus(id, priorStatus[id] ?? "Stopped");
    } else if (failed.has(id)) {
      deps.setStatus(id, "Error");
    }
  }

  if (activeInWorkspace && deleted.has(activeSessionId!)) deps.navigateHome();

  if (result.failed && result.failed.length > 0) {
    deps.notify?.error?.("Some sessions could not be deleted");
    return;
  }
  // `messages` carries notes like a kept scratch path.
  deps.notify?.info?.(result.messages?.[0] ?? (ids.length > 1 ? "Sessions deleted" : "Session deleted"));
}

/** Returns true iff all succeeded. */
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

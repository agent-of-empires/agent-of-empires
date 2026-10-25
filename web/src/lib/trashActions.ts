// Trash, restore, and confirmed-deletion transitions.

import { deleteWorkspace, restoreSession, trashSession } from "./api";
import type { DeleteSessionOptions, DeleteWorkspaceResult, TrashPurgeConsent } from "./api";
import type { SessionResponse, SessionStatus, Workspace } from "./types";

const BUSY_MESSAGE = "Some sessions are busy with another operation; nothing changed, retry when it finishes";

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
  clearDeleting: (sessions: SessionResponse[]) => void;
  /** Runs only after the server confirms the delete. */
  purgeLocal: (id: string) => void;
  navigateHome: () => void;
  notify: Notifier | null;
}

/** Atomic delete of `sessions`; the server keeps a worktree any other session still uses. Local cleanup and the redirect home only follow confirmed deletions. */
export async function deleteWorkspaceSessions(
  sessions: SessionResponse[],
  options: Omit<DeleteSessionOptions, "trash_consents">,
  activeSessionId: string | null,
  deps: DeleteWorkspaceDeps,
): Promise<DeleteWorkspaceResult> {
  if (sessions.length === 0) return { ok: true, deleted: [], kept: [] };
  const ids = sessions.map((s) => s.id);
  const activeInWorkspace = activeSessionId != null && ids.includes(activeSessionId);
  const trash_consents: Record<string, TrashPurgeConsent> = {};
  for (const session of sessions) {
    if (!session.trashed_at && !options.expected_trash) continue;
    if (
      !session.trashed_at ||
      !session.trash_lifecycle_id ||
      !Number.isSafeInteger(session.lifecycle_generation) ||
      session.lifecycle_generation < 0
    ) {
      const error = "The original trash lifecycle is unavailable. Refresh before selecting sessions to purge.";
      deps.notify?.error?.(error);
      return { ok: false, error };
    }
    trash_consents[session.id] = {
      expected_trashed_at: session.trashed_at,
      expected_trash_lifecycle_id: session.trash_lifecycle_id,
      expected_generation: session.lifecycle_generation,
    };
  }

  for (const id of ids) deps.setStatus(id, "Deleting");

  const result = await deleteWorkspace(ids, { ...options, trash_consents });
  if (!result.ok && result.busy?.length) {
    deps.clearDeleting(sessions);
    deps.notify?.error?.(BUSY_MESSAGE);
    return result;
  }
  if (!result.ok) {
    for (const id of ids) deps.setStatus(id, "Error");
    deps.notify?.error?.(result.error || "Failed to delete session");
    return result;
  }

  const deleted = new Set(result.deleted ?? []);
  const failed = new Set((result.failed ?? []).map((f) => f.id));
  const busy = new Set((result.busy ?? []).map((b) => b.id));
  const retained = sessions.filter((session) => !deleted.has(session.id) && !failed.has(session.id));
  const kept = retained.filter((session) => !busy.has(session.id)).map((session) => session.id);
  deps.clearDeleting(retained);
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
    return { ...result, kept };
  }
  if (busy.size > 0) {
    deps.notify?.error?.(BUSY_MESSAGE);
    return { ...result, kept };
  }
  deps.notify?.info?.(
    retained.length > 0
      ? "Some selected sessions changed and were not deleted"
      : (result.messages?.[0] ?? (ids.length > 1 ? "Sessions deleted" : "Session deleted")),
  );
  return { ...result, kept };
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

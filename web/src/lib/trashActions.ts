// Trash, restore, and confirmed-deletion transitions.

import { deleteWorkspace, restoreSession, trashSession } from "./api";
import type {
  DeleteSessionOptions,
  DeleteWorkspaceResult,
  SessionMutation,
  TrashRetryConsent,
  TrashPurgeConsent,
} from "./api";
import type { SessionResponse, SessionStatus, Workspace } from "./types";

const BUSY_MESSAGE = "Some sessions are busy with another operation and were not changed; retry them when it finishes";

/** Restore only trashed members of the workspace containing `sessionId`; live siblings stay untouched. */
export function trashedWorkspaceRestoreIds(workspaces: Workspace[], sessionId: string): string[] {
  const ws = workspaces.find((w) => w.sessions.some((s) => s.id === sessionId));
  return ws ? ws.sessions.filter((session) => session.trashed_at != null).map((session) => session.id) : [sessionId];
}

export interface Notifier {
  error?: (message: string) => void;
  info?: (message: string) => void;
}

export interface TrashRecoveryTarget extends TrashRetryConsent {
  id: string;
  title: string;
  created_at: string;
  epoch: string;
  expected_trashed_at: string;
  expected_trash_lifecycle_id: string;
}

export function hasPendingTrash(session: SessionResponse): boolean {
  return Boolean(
    session.trashed_at &&
    session.lifecycle_reservation?.op === "trash" &&
    session.lifecycle_reservation.generation === session.lifecycle_generation,
  );
}

export function trashRecoveryTarget(session: SessionResponse, epoch: string | null): TrashRecoveryTarget | null {
  if (
    !epoch ||
    !hasPendingTrash(session) ||
    !session.trashed_at ||
    !session.trash_lifecycle_id ||
    !session.profile ||
    !Number.isSafeInteger(session.lifecycle_generation) ||
    session.lifecycle_generation < 0
  )
    return null;
  return {
    id: session.id,
    title: session.title,
    created_at: session.created_at,
    epoch,
    expected_profile: session.profile,
    expected_generation: session.lifecycle_generation,
    expected_trashed_at: session.trashed_at,
    expected_trash_lifecycle_id: session.trash_lifecycle_id,
  };
}

export function trashRecoveryTargetIsCurrent(
  session: SessionResponse | undefined,
  target: TrashRecoveryTarget,
  epoch: string | null,
): boolean {
  return Boolean(
    session &&
    hasPendingTrash(session) &&
    epoch === target.epoch &&
    session.id === target.id &&
    session.profile === target.expected_profile &&
    session.created_at === target.created_at &&
    session.lifecycle_generation === target.expected_generation &&
    session.trashed_at === target.expected_trashed_at &&
    session.trash_lifecycle_id === target.expected_trash_lifecycle_id,
  );
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
      res.warnings?.forEach((warning) => deps.notify?.info?.(warning));
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
    deps.notify?.error?.(
      busy.size > 0
        ? "Some sessions could not be deleted, and some are busy with another operation; retry the busy ones when it finishes"
        : "Some sessions could not be deleted",
    );
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
export async function restoreSessions(
  ids: string[],
  deps: { applySessionMutation: (mutation: SessionMutation) => void; notify: Notifier | null },
): Promise<boolean> {
  let anyFailed = false;
  for (const id of ids) {
    const res = await restoreSession(id);
    if (res) {
      deps.applySessionMutation(res);
      res.session.warnings?.forEach((warning) => deps.notify?.info?.(warning));
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

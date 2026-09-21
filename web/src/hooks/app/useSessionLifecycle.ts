// Delete, trash, stop, start, and view-switch flows for sidebar sessions, including their confirm dialogs.

import { useCallback, useRef, useState } from "react";
import type { NavigateFunction } from "react-router-dom";
import { clearStoredComments } from "../../components/diff/comments/storage";
import { clearDraft } from "../../lib/acpDrafts";
import { acpDisable, acpEnable, startSession, stopSession, type DeleteSessionOptions } from "../../lib/api";
import { toastBus } from "../../lib/toastBus";
import {
  deleteWorkspaceSessions,
  restoreSessions,
  trashSessions,
  workspaceCleanupDefaults,
  type Notifier,
} from "../../lib/trashActions";
import type { SessionResponse, Workspace } from "../../lib/types";
import { clearAcpCache } from "../useAcpSession";

function purgeLocal(id: string): void {
  clearAcpCache(id);
  clearDraft(id);
  clearStoredComments(id);
}

interface Options {
  workspaces: Workspace[];
  trashedWorkspaces: Workspace[];
  activeSessionId: string | null;
  setSessionStatus: (id: string, status: SessionResponse["status"]) => void;
  applySession: (session: SessionResponse) => void;
  navigate: NavigateFunction;
}

export function useSessionLifecycle({
  workspaces,
  trashedWorkspaces,
  activeSessionId,
  setSessionStatus,
  applySession,
  navigate,
}: Options) {
  const [deletingWorkspaceId, setDeletingWorkspaceId] = useState<string | null>(null);
  const [stoppingWorkspaceId, setStoppingWorkspaceId] = useState<string | null>(null);
  const [switchViewTarget, setSwitchViewTarget] = useState<{ sessionId: string; toStructured: boolean } | null>(null);

  const deletingWorkspace = deletingWorkspaceId ? workspaces.find((w) => w.id === deletingWorkspaceId) : null;
  const deletingSessions = deletingWorkspace?.sessions ?? [];
  const deletingSession = deletingWorkspace?.sessions[0] ?? null;
  const deletingDefaultToTrash = deletingSessions.some((s) => !s.trashed_at && s.cleanup_defaults.delete_to_trash);
  const deleting = deletingSession
    ? {
        session: deletingSession,
        sessions: deletingSessions,
        defaultToTrash: deletingDefaultToTrash,
        cleanupDefaults: { delete_to_trash: deletingDefaultToTrash, ...workspaceCleanupDefaults(deletingSessions) },
        branchName: deletingSessions.find((s) => s.branch)?.branch ?? deletingSession.branch ?? null,
      }
    : null;

  const confirmDelete = async (options: DeleteSessionOptions) => {
    if (!deletingWorkspace) return;
    setDeletingWorkspaceId(null);
    await deleteWorkspaceSessions(deletingWorkspace.sessions, options, activeSessionId, {
      setStatus: setSessionStatus,
      purgeLocal,
      navigateHome: () => navigate("/"),
      notify: toastBus.handler,
    });
  };

  const emptyingTrashRef = useRef(false);
  const emptyTrash = useCallback(async () => {
    if (trashedWorkspaces.length === 0 || emptyingTrashRef.current) return;
    emptyingTrashRef.current = true;
    let anyFailed = false;
    const notify: Notifier = {
      error: () => {
        anyFailed = true;
      },
      info: () => {},
    };
    try {
      for (const ws of trashedWorkspaces) {
        await deleteWorkspaceSessions(
          ws.sessions,
          { ...workspaceCleanupDefaults(ws.sessions), force_delete: true },
          activeSessionId,
          { setStatus: setSessionStatus, purgeLocal, navigateHome: () => navigate("/"), notify },
        );
      }
    } finally {
      emptyingTrashRef.current = false;
    }
    toastBus.handler?.[anyFailed ? "error" : "info"](
      anyFailed ? "Some trashed sessions could not be deleted" : "Emptied trash",
    );
  }, [trashedWorkspaces, activeSessionId, setSessionStatus, navigate]);

  const confirmTrash = async () => {
    if (!deletingWorkspace) return;
    const ids = deletingWorkspace.sessions.map((s) => s.id);
    if (ids.length === 0) return;
    setDeletingWorkspaceId(null);
    for (const id of ids) setSessionStatus(id, "Stopped");
    if (activeSessionId != null && ids.includes(activeSessionId)) navigate("/");
    await trashSessions(ids, {
      applySession,
      onError: (id) => setSessionStatus(id, "Error"),
      notify: toastBus.handler,
    });
  };

  const restore = useCallback(
    (sessionIds: string[]) => restoreSessions(sessionIds, { applySession, notify: toastBus.handler }),
    [applySession],
  );

  const stoppingSession = stoppingWorkspaceId
    ? (workspaces.find((w) => w.id === stoppingWorkspaceId)?.sessions[0] ?? null)
    : null;

  // Show the outcome optimistically, and fall back to Error when the daemon refuses.
  const run = useCallback(
    async (sessionId: string, pending: SessionResponse["status"], call: () => Promise<unknown>, verb: string) => {
      setSessionStatus(sessionId, pending);
      if (await call()) {
        toastBus.handler?.info(`Session ${verb}ed`);
        return;
      }
      setSessionStatus(sessionId, "Error");
      toastBus.handler?.error(`Failed to ${verb} session`);
    },
    [setSessionStatus],
  );

  const confirmStop = useCallback(async () => {
    if (!stoppingSession) return;
    const { id } = stoppingSession;
    setStoppingWorkspaceId(null);
    await run(id, "Stopped", () => stopSession(id), "stop");
  }, [stoppingSession, run]);

  const start = useCallback(
    async (workspaceId: string) => {
      const session = workspaces.find((w) => w.id === workspaceId)?.sessions[0];
      if (!session) return;
      await run(session.id, "Starting", () => startSession(session.id), "start");
    },
    [workspaces, run],
  );

  const switchViewSession = switchViewTarget
    ? (workspaces.flatMap((w) => w.sessions).find((s) => s.id === switchViewTarget.sessionId) ?? null)
    : null;

  const requestSwitchView = useCallback((sessionId: string, toStructured: boolean) => {
    setSwitchViewTarget({ sessionId, toStructured });
  }, []);

  const confirmSwitchView = useCallback(async () => {
    if (!switchViewTarget) return;
    const { sessionId, toStructured } = switchViewTarget;
    const result = toStructured ? await acpEnable(sessionId) : await acpDisable(sessionId);
    setSwitchViewTarget(null);
    const target = toStructured ? "structured view" : "terminal";
    if (result) toastBus.handler?.info(`Switched to ${target}`);
    else toastBus.handler?.error(`Failed to switch to ${target}`);
  }, [switchViewTarget]);

  return {
    deletingWorkspaceId,
    setDeletingWorkspaceId,
    deleting,
    confirmDelete,
    confirmTrash,
    emptyTrash,
    restore,
    stoppingWorkspaceId,
    setStoppingWorkspaceId,
    stoppingSession,
    confirmStop,
    start,
    switchViewTarget,
    setSwitchViewTarget,
    switchViewSession,
    requestSwitchView,
    confirmSwitchView,
  };
}

import { useCallback, useState } from "react";

import { setSessionArchive, setSessionPin, setSessionSnooze, setSessionUnread } from "../lib/api";
import { reportError } from "../lib/toastBus";
import {
  EMPTY_OPTIMISTIC,
  makeOptimisticSnoozedUntil,
  reconcileOptimistic,
  withOverride,
  type OptimisticTriage,
} from "../lib/sidebarOptimistic";
import type { Workspace } from "../lib/types";

export interface TriageResult {
  workspaceId: string;
  ok: boolean;
  skipped?: boolean;
}

function reportFailure(result: TriageResult, message: string): void {
  if (!result.ok && !result.skipped) reportError(message);
}

export function useSidebarTriage(workspaces: readonly Workspace[]) {
  const [overlay, setOverlay] = useState<Map<string, OptimisticTriage>>(() => new Map());
  const [trackedWorkspaces, setTrackedWorkspaces] = useState(workspaces);
  if (workspaces !== trackedWorkspaces) {
    setTrackedWorkspaces(workspaces);
    setOverlay((prev) => reconcileOptimistic(prev, workspaces));
  }

  const setOverride = useCallback((workspaceId: string, patch: Partial<OptimisticTriage>) => {
    setOverlay((prev) => {
      const next = new Map(prev);
      next.set(workspaceId, withOverride(prev.get(workspaceId), patch));
      return next;
    });
  }, []);

  const optimisticFor = useCallback(
    (workspaceId: string): OptimisticTriage => overlay.get(workspaceId) ?? EMPTY_OPTIMISTIC,
    [overlay],
  );

  const triage = useCallback(
    async (
      ws: Workspace,
      optimistic: Partial<OptimisticTriage>,
      revert: Partial<OptimisticTriage>,
      call: (sessionId: string) => Promise<unknown>,
    ): Promise<TriageResult> => {
      const sessionId = ws.sessions[0]?.id;
      if (!sessionId) return { workspaceId: ws.id, ok: false, skipped: true };
      setOverride(ws.id, optimistic);
      if (await call(sessionId)) return { workspaceId: ws.id, ok: true };
      setOverride(ws.id, revert);
      return { workspaceId: ws.id, ok: false };
    },
    [setOverride],
  );

  const pin = useCallback(
    (ws: Workspace, pinned: boolean) => triage(ws, { pinned }, { pinned: null }, (id) => setSessionPin(id, pinned)),
    [triage],
  );

  const archive = useCallback(
    (ws: Workspace, archived: boolean) =>
      triage(ws, { archived }, { archived: null }, (id) => setSessionArchive(id, archived)),
    [triage],
  );

  const snooze = useCallback(
    (ws: Workspace, minutes: number | null) =>
      triage(
        ws,
        { snoozedUntil: minutes == null ? null : makeOptimisticSnoozedUntil(minutes) },
        { snoozedUntil: undefined },
        (id) => setSessionSnooze(id, minutes),
      ),
    [triage],
  );

  const unread = useCallback(
    (ws: Workspace, markUnread: boolean) =>
      triage(ws, { unread: markUnread }, { unread: null }, (id) => setSessionUnread(id, markUnread)),
    [triage],
  );

  const pinToggle = useCallback(
    (ws: Workspace, pinned: boolean) => {
      void pin(ws, pinned).then((r) => reportFailure(r, pinned ? "Failed to pin session" : "Failed to unpin session"));
    },
    [pin],
  );

  const archiveToggle = useCallback(
    (ws: Workspace, archived: boolean) => {
      void archive(ws, archived).then((r) =>
        reportFailure(r, archived ? "Failed to archive session" : "Failed to unarchive session"),
      );
    },
    [archive],
  );

  const snoozeOne = useCallback(
    (ws: Workspace, minutes: number | null) => {
      void snooze(ws, minutes).then((r) =>
        reportFailure(r, minutes == null ? "Failed to unsnooze session" : "Failed to snooze session"),
      );
    },
    [snooze],
  );

  const unreadToggle = useCallback(
    (ws: Workspace, markUnread: boolean) => {
      void unread(ws, markUnread).then((r) =>
        reportFailure(r, markUnread ? "Failed to mark unread" : "Failed to mark read"),
      );
    },
    [unread],
  );

  // Serial: each PATCH rewrites the profile's session list, so concurrent calls would race.
  const runBulk = useCallback(async (wss: readonly Workspace[], action: (ws: Workspace) => Promise<TriageResult>) => {
    const results: TriageResult[] = [];
    for (const ws of wss) results.push(await action(ws));
    return results;
  }, []);

  const bulkPin = useCallback(
    (wss: readonly Workspace[], pinned: boolean) => runBulk(wss, (ws) => pin(ws, pinned)),
    [runBulk, pin],
  );
  const bulkArchive = useCallback(
    (wss: readonly Workspace[], archived: boolean) => runBulk(wss, (ws) => archive(ws, archived)),
    [runBulk, archive],
  );
  const bulkSnooze = useCallback(
    (wss: readonly Workspace[], minutes: number | null) => runBulk(wss, (ws) => snooze(ws, minutes)),
    [runBulk, snooze],
  );

  return {
    optimisticFor,
    pinToggle,
    archiveToggle,
    snooze: snoozeOne,
    unreadToggle,
    bulkPin,
    bulkArchive,
    bulkSnooze,
  };
}

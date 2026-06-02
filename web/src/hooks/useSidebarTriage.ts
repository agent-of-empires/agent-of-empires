import { useCallback, useEffect, useState } from "react";

import {
  setSessionArchive,
  setSessionPin,
  setSessionSnooze,
} from "../lib/api";
import { reportError } from "../lib/toastBus";
import {
  EMPTY_OPTIMISTIC,
  makeOptimisticSnoozedUntil,
  reconcileOptimistic,
  withOverride,
  type OptimisticTriage,
} from "../lib/sidebarOptimistic";
import type { Workspace } from "../lib/types";

/** Outcome of one triage mutation, used to build the bulk summary toast. */
export interface TriageResult {
  workspaceId: string;
  ok: boolean;
  /** Set when the workspace had no session to act on. */
  skipped?: boolean;
}

/** Sidebar triage controller: owns the optimistic overlay (keyed by workspace
 *  id) and the single-id PATCH calls for pin / archive / snooze, for both
 *  single-row and bulk actions. Lifted out of `SessionRow` so a bulk action
 *  can drive many rows from one place rather than reaching into N independent
 *  row components. Triage always targets the workspace's primary session
 *  (`sessions[0]`), matching the prior row-level behavior. See #1724. */
export function useSidebarTriage(workspaces: readonly Workspace[]) {
  const [overlay, setOverlay] = useState<Map<string, OptimisticTriage>>(
    () => new Map(),
  );

  // Drop overrides the server has caught up to. reconcileOptimistic returns
  // the same map reference when nothing changed, so this never loops.
  useEffect(() => {
    setOverlay((prev) => reconcileOptimistic(prev, workspaces));
  }, [workspaces]);

  const setOverride = useCallback(
    (workspaceId: string, patch: Partial<OptimisticTriage>) => {
      setOverlay((prev) => {
        const next = new Map(prev);
        next.set(workspaceId, withOverride(prev.get(workspaceId), patch));
        return next;
      });
    },
    [],
  );

  const optimisticFor = useCallback(
    (workspaceId: string): OptimisticTriage =>
      overlay.get(workspaceId) ?? EMPTY_OPTIMISTIC,
    [overlay],
  );

  const pin = useCallback(
    async (ws: Workspace, pinned: boolean): Promise<TriageResult> => {
      const sessionId = ws.sessions[0]?.id;
      if (!sessionId) return { workspaceId: ws.id, ok: false, skipped: true };
      setOverride(ws.id, { pinned });
      const result = await setSessionPin(sessionId, pinned);
      if (!result) {
        setOverride(ws.id, { pinned: null });
        return { workspaceId: ws.id, ok: false };
      }
      return { workspaceId: ws.id, ok: true };
    },
    [setOverride],
  );

  const archive = useCallback(
    async (ws: Workspace, archived: boolean): Promise<TriageResult> => {
      const sessionId = ws.sessions[0]?.id;
      if (!sessionId) return { workspaceId: ws.id, ok: false, skipped: true };
      setOverride(ws.id, { archived });
      const result = await setSessionArchive(sessionId, archived);
      if (!result) {
        setOverride(ws.id, { archived: null });
        return { workspaceId: ws.id, ok: false };
      }
      return { workspaceId: ws.id, ok: true };
    },
    [setOverride],
  );

  const snooze = useCallback(
    async (ws: Workspace, minutes: number | null): Promise<TriageResult> => {
      const sessionId = ws.sessions[0]?.id;
      if (!sessionId) return { workspaceId: ws.id, ok: false, skipped: true };
      const optimisticUntil =
        minutes == null ? null : makeOptimisticSnoozedUntil(minutes);
      setOverride(ws.id, { snoozedUntil: optimisticUntil });
      const result = await setSessionSnooze(sessionId, minutes);
      if (!result) {
        setOverride(ws.id, { snoozedUntil: undefined });
        return { workspaceId: ws.id, ok: false };
      }
      return { workspaceId: ws.id, ok: true };
    },
    [setOverride],
  );

  // Single-row handlers surface a toast on failure (the bulk path reports a
  // single summary toast instead, so these don't).
  const pinToggle = useCallback(
    (ws: Workspace, pinned: boolean) => {
      void pin(ws, pinned).then((r) => {
        if (!r.ok && !r.skipped) {
          reportError(
            pinned ? "Failed to pin session" : "Failed to unpin session",
          );
        }
      });
    },
    [pin],
  );

  const archiveToggle = useCallback(
    (ws: Workspace, archived: boolean) => {
      void archive(ws, archived).then((r) => {
        if (!r.ok && !r.skipped) {
          reportError(
            archived
              ? "Failed to archive session"
              : "Failed to unarchive session",
          );
        }
      });
    },
    [archive],
  );

  const snoozeOne = useCallback(
    (ws: Workspace, minutes: number | null) => {
      void snooze(ws, minutes).then((r) => {
        if (!r.ok && !r.skipped) {
          reportError(
            minutes == null
              ? "Failed to unsnooze session"
              : "Failed to snooze session",
          );
        }
      });
    },
    [snooze],
  );

  // Bulk fan-out. Serial on purpose: each single-id PATCH re-persists the
  // whole profile's session list, so firing them concurrently could race on
  // that write. For the handful of rows a user bulk-triages against a local
  // server this is sub-second, and it keeps the per-session semantics
  // (lock, persist-first, archive side effects) exactly as the single path.
  // Best-effort, not atomic: a failure rolls back only its own row. See
  // #1724. Swapping this loop for a real bulk endpoint later is a localized
  // change.
  const runBulk = useCallback(
    async (
      workspaces: readonly Workspace[],
      action: (ws: Workspace) => Promise<TriageResult>,
    ): Promise<TriageResult[]> => {
      const results: TriageResult[] = [];
      for (const ws of workspaces) {
        results.push(await action(ws));
      }
      return results;
    },
    [],
  );

  const bulkPin = useCallback(
    (wss: readonly Workspace[], pinned: boolean) =>
      runBulk(wss, (ws) => pin(ws, pinned)),
    [runBulk, pin],
  );
  const bulkArchive = useCallback(
    (wss: readonly Workspace[], archived: boolean) =>
      runBulk(wss, (ws) => archive(ws, archived)),
    [runBulk, archive],
  );
  const bulkSnooze = useCallback(
    (wss: readonly Workspace[], minutes: number | null) =>
      runBulk(wss, (ws) => snooze(ws, minutes)),
    [runBulk, snooze],
  );

  return {
    optimisticFor,
    pinToggle,
    archiveToggle,
    snooze: snoozeOne,
    bulkPin,
    bulkArchive,
    bulkSnooze,
  };
}

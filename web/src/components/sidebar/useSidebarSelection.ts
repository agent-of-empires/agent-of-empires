import { useCallback, useEffect, useLayoutEffect, useMemo, useReducer, useRef, useState } from "react";
import type { SidebarWorkspaceView } from "../../lib/sidebarGroups";
import type { Workspace } from "../../lib/types";
import { EMPTY_SELECTION, classifyClick, selectionReducer } from "../../lib/sidebarSelection";
import { bucketSelectionForBulk, summarizeBulkResults } from "../../lib/sidebarBulk";
import { reportError, reportInfo } from "../../lib/toastBus";
import type { useSidebarTriage } from "../../hooks/useSidebarTriage";
import type { RowActivate, RowBulkApi } from "./types";

type Triage = ReturnType<typeof useSidebarTriage>;

/** Ephemeral multi-select, bulk triage, and click routing (navigate, toggle, or range) for sidebar rows. */
export function useSidebarSelection({
  rows,
  orderedIds,
  triage,
  readOnly,
  activeId,
  activeSessionId,
  onSelect,
}: {
  rows: readonly SidebarWorkspaceView[];
  orderedIds: string[];
  triage: Triage;
  readOnly?: boolean;
  activeId: string | null;
  activeSessionId: string | null;
  onSelect: (workspaceId: string, sessionId: string | null) => void;
}) {
  const [selection, dispatch] = useReducer(selectionReducer, EMPTY_SELECTION);
  const [optimisticActive, setOptimisticActive] = useState<{
    id: string;
    sessionId: string | null;
    fromActiveId: string | null;
    fromActiveSessionId: string | null;
  } | null>(null);
  // Drop the hint once the route moves, on either half of the pair: a sibling session opened by
  // another route shares the workspace id, so it must still invalidate the hint.
  const hintApplies =
    optimisticActive !== null &&
    optimisticActive.fromActiveId === activeId &&
    optimisticActive.fromActiveSessionId === activeSessionId;
  if (optimisticActive && !hintApplies) setOptimisticActive(null);
  const displayedActiveId = optimisticActive && hintApplies ? optimisticActive.id : activeId;
  // A status slice holds one session, so an active row must match it, not just the workspace id.
  const displayedActiveSessionId = optimisticActive && hintApplies ? optimisticActive.sessionId : activeSessionId;

  // Selection is keyed by the rendered row: the status axis slices one workspace into a row per
  // status, and each row's actions belong to its own session.
  const byKey = useMemo(() => new Map(rows.map((row) => [row.key, row])), [rows]);
  const existingIds = useMemo(() => new Set(byKey.keys()), [byKey]);
  // Prune only vanished rows; collapsing or filtering keeps the selection.
  useEffect(() => dispatch({ type: "prune", validIds: existingIds }), [existingIds]);

  // Read-only viewers cannot act on a selection, so none may accumulate.
  if (readOnly && selection.selectedIds.size > 0) dispatch({ type: "clear" });

  const selectedWorkspaces = useMemo(
    () => rows.filter((row) => selection.selectedIds.has(row.key)).map((row) => row.workspace),
    [rows, selection.selectedIds],
  );

  const runBulkAction = useCallback(
    async (verb: string, run: () => Promise<readonly { ok: boolean; skipped?: boolean }[]>) => {
      const results = await run();
      const summary = summarizeBulkResults(verb, results);
      if (results.some((r) => !r.ok && !r.skipped)) reportError(summary);
      else reportInfo(summary);
      dispatch({ type: "clear" });
    },
    [],
  );
  const onBulkPin = useCallback(
    (wss: Workspace[], pinned: boolean) =>
      void runBulkAction(pinned ? "Pinned" : "Unpinned", () => triage.bulkPin(wss, pinned)),
    [runBulkAction, triage],
  );
  const onBulkArchive = useCallback(
    (wss: Workspace[], archived: boolean) =>
      void runBulkAction(archived ? "Archived" : "Unarchived", () => triage.bulkArchive(wss, archived)),
    [runBulkAction, triage],
  );
  const onBulkSnooze = useCallback(
    (wss: Workspace[], minutes: number | null) =>
      void runBulkAction(minutes == null ? "Unsnoozed" : "Snoozed", () => triage.bulkSnooze(wss, minutes)),
    [runBulkAction, triage],
  );

  // Live state behind a ref keeps `rowBulkApi` stable so memoized rows do not re-render.
  const live = {
    selectedIds: selection.selectedIds,
    selectedWorkspaces,
    triage,
    readOnly,
    onBulkPin,
    onBulkArchive,
    onBulkSnooze,
  };
  const liveRef = useRef(live);
  useLayoutEffect(() => {
    liveRef.current = live;
  });
  const rowBulkApi = useMemo<RowBulkApi>(
    () => ({
      prepareScope: (rowKey) => {
        const { selectedIds, selectedWorkspaces, triage, readOnly } = liveRef.current;
        if (selectedIds.has(rowKey) && selectedIds.size > 1) {
          return {
            kind: "bulk",
            count: selectedWorkspaces.length,
            buckets: bucketSelectionForBulk(selectedWorkspaces, triage.optimisticFor),
          };
        }
        // Right-clicking outside the selection makes that row the sole selection, without navigating.
        if (!readOnly && !(selectedIds.has(rowKey) && selectedIds.size === 1)) {
          dispatch({ type: "select-only", id: rowKey });
        }
        return { kind: "single" };
      },
      pin: (wss, pinned) => liveRef.current.onBulkPin(wss, pinned),
      archive: (wss, archived) => liveRef.current.onBulkArchive(wss, archived),
      snooze: (wss, minutes) => liveRef.current.onBulkSnooze(wss, minutes),
    }),
    [],
  );

  // Escape clears the selection unless typing, or unless a popup already used it to close itself.
  useEffect(() => {
    if (selection.selectedIds.size === 0) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.defaultPrevented) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
      dispatch({ type: "clear" });
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [selection.selectedIds.size]);

  // Selection uses row identity; navigation uses the clicked row's workspace and session.
  const handleRowActivate = useCallback<RowActivate>(
    (rowKey, e, sessionId) => {
      const intent = readOnly ? "navigate" : classifyClick(e);
      if (intent === "navigate") {
        dispatch(readOnly ? { type: "clear" } : { type: "navigate", id: rowKey });
        // Rows outside the group lists (the Trash panel) key themselves by workspace id.
        const workspaceId = byKey.get(rowKey)?.workspace.id ?? rowKey;
        setOptimisticActive({
          id: workspaceId,
          sessionId,
          fromActiveId: activeId,
          fromActiveSessionId: activeSessionId,
        });
        onSelect(workspaceId, sessionId);
      } else if (intent === "toggle") {
        dispatch({ type: "toggle", id: rowKey });
      } else {
        dispatch({ type: "range", targetId: rowKey, orderedIds, additive: intent === "additive-range" });
      }
    },
    [activeId, activeSessionId, byKey, onSelect, orderedIds, readOnly],
  );

  const isSelected = (key: string) => !readOnly && selection.selectedIds.has(key);
  return {
    displayedActiveId,
    displayedActiveSessionId,
    isSelected,
    rowBulkApi,
    handleRowActivate,
    onBulkArchive,
  };
}

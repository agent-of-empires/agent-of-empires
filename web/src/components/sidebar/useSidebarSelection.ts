import { useCallback, useEffect, useLayoutEffect, useMemo, useReducer, useRef, useState } from "react";
import type { Workspace } from "../../lib/types";
import { EMPTY_SELECTION, classifyClick, selectionReducer } from "../../lib/sidebarSelection";
import { bucketSelectionForBulk, summarizeBulkResults } from "../../lib/sidebarBulk";
import { reportError, reportInfo } from "../../lib/toastBus";
import type { useSidebarTriage } from "../../hooks/useSidebarTriage";
import type { RowActivate, RowBulkApi } from "./types";

type Triage = ReturnType<typeof useSidebarTriage>;

/** Ephemeral multi-select, bulk triage, and click routing (navigate, toggle, or range) for sidebar rows. */
export function useSidebarSelection({
  allWorkspaces,
  orderedIds,
  triage,
  readOnly,
  activeId,
  onSelect,
}: {
  allWorkspaces: Workspace[];
  orderedIds: string[];
  triage: Triage;
  readOnly?: boolean;
  activeId: string | null;
  onSelect: (workspaceId: string, sessionId: string | null) => void;
}) {
  const [selection, dispatch] = useReducer(selectionReducer, EMPTY_SELECTION);
  const [optimisticActive, setOptimisticActive] = useState<{ id: string; fromActiveId: string | null } | null>(null);
  // Drop the hint once activeId moves, so navigating back to `fromActiveId` cannot revive a stale id.
  if (optimisticActive && optimisticActive.fromActiveId !== activeId) setOptimisticActive(null);
  const displayedActiveId = optimisticActive?.fromActiveId === activeId ? optimisticActive.id : activeId;

  // Prune only vanished workspaces; collapsing or filtering keeps the selection.
  const existingIds = useMemo(() => new Set(allWorkspaces.map((w) => w.id)), [allWorkspaces]);
  useEffect(() => dispatch({ type: "prune", validIds: existingIds }), [existingIds]);

  // Read-only viewers cannot act on a selection, so none may accumulate.
  if (readOnly && selection.selectedIds.size > 0) dispatch({ type: "clear" });

  const selectedWorkspaces = useMemo(
    () => allWorkspaces.filter((w) => selection.selectedIds.has(w.id)),
    [allWorkspaces, selection.selectedIds],
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
      prepareScope: (ws) => {
        const { selectedIds, selectedWorkspaces, triage, readOnly } = liveRef.current;
        if (selectedIds.has(ws.id) && selectedIds.size > 1) {
          return {
            kind: "bulk",
            count: selectedWorkspaces.length,
            buckets: bucketSelectionForBulk(selectedWorkspaces, triage.optimisticFor),
          };
        }
        // Right-clicking outside the selection makes that row the sole selection, without navigating.
        if (!readOnly && !(selectedIds.has(ws.id) && selectedIds.size === 1)) {
          dispatch({ type: "select-only", id: ws.id });
        }
        return { kind: "single" };
      },
      pin: (wss, pinned) => liveRef.current.onBulkPin(wss, pinned),
      archive: (wss, archived) => liveRef.current.onBulkArchive(wss, archived),
      snooze: (wss, minutes) => liveRef.current.onBulkSnooze(wss, minutes),
    }),
    [],
  );

  // Escape clears the selection unless typing.
  useEffect(() => {
    if (selection.selectedIds.size === 0) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
      dispatch({ type: "clear" });
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [selection.selectedIds.size]);

  // Selection uses workspace identity; navigation uses the clicked row's session.
  const handleRowActivate = useCallback<RowActivate>(
    (workspaceId, e, sessionId) => {
      const intent = readOnly ? "navigate" : classifyClick(e);
      if (intent === "navigate") {
        dispatch(readOnly ? { type: "clear" } : { type: "navigate", id: workspaceId });
        setOptimisticActive({ id: workspaceId, fromActiveId: activeId });
        onSelect(workspaceId, sessionId);
      } else if (intent === "toggle") {
        dispatch({ type: "toggle", id: workspaceId });
      } else {
        dispatch({ type: "range", targetId: workspaceId, orderedIds, additive: intent === "additive-range" });
      }
    },
    [activeId, onSelect, orderedIds, readOnly],
  );

  const isSelected = (id: string) => !readOnly && selection.selectedIds.has(id);
  return { displayedActiveId, isSelected, rowBulkApi, handleRowActivate, onBulkArchive };
}

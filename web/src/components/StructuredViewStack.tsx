import { useEffect, useMemo, useState } from "react";
import type { SessionResponse } from "../lib/types";
import type { FileRef } from "../lib/fileRef";
import { normalizePersistentStructuredViewLimit } from "../lib/persistentStructuredViews";
import { StructuredView } from "./acp/StructuredView";

interface Props {
  activeSessionId: string | null;
  sessions: SessionResponse[];
  persistent: boolean;
  maxPersistentStructuredViews?: number;
  /** On desktop this is true whenever the active session is structured.
   *  On mobile it is true only when `view === "agent"`. The stack uses it to
   *  decide whether the active structured session is actually interactive. */
  visible: boolean;
  onOpenFileRef?: (ref: FileRef) => void;
  onOpenAgentsPane?: () => void;
  /** Restore a trashed session by id. If absent, trashed sessions render
   *  their banner read-only. */
  onRestoreSession?: (sessionId: string) => Promise<boolean> | void;
}

export function StructuredViewStack({
  activeSessionId,
  sessions,
  persistent,
  maxPersistentStructuredViews,
  visible,
  onOpenFileRef,
  onOpenAgentsPane,
  onRestoreSession,
}: Props) {
  const [recentIds, setRecentIds] = useState<string[]>([]);
  const limit = normalizePersistentStructuredViewLimit(maxPersistentStructuredViews);
  const sessionsById = useMemo(() => new Map(sessions.map((session) => [session.id, session])), [sessions]);

  useEffect(() => {
    let cancelled = false;
    queueMicrotask(() => {
      if (cancelled) return;
      if (!persistent) {
        setRecentIds([]);
        return;
      }
      setRecentIds((ids) => {
        // Drop sessions whose view is no longer structured and remove duplicates.
        const structuredIds = ids.filter((id) => sessionsById.get(id)?.view === "structured");
        if (!activeSessionId) {
          return structuredIds;
        }
        const activeSession = sessionsById.get(activeSessionId);
        if (!activeSession || activeSession.view !== "structured") {
          // Active session is not structured; keep recent structured sessions warm
          // but do not add the active id.
          return structuredIds.filter((id) => id !== activeSessionId).slice(0, limit);
        }
        const inactive = structuredIds.filter((id) => id !== activeSessionId);
        const next = [activeSessionId, ...inactive].slice(0, limit);
        return next.join("\0") === ids.join("\0") ? ids : next;
      });
    });
    return () => {
      cancelled = true;
    };
  }, [activeSessionId, limit, persistent, sessionsById]);

  const activeStructuredId =
    activeSessionId && sessionsById.get(activeSessionId)?.view === "structured" ? activeSessionId : null;

  const visibleIds = useMemo(() => {
    const retained = recentIds.filter((id) => {
      const session = sessionsById.get(id);
      return session && session.view === "structured" && id !== activeStructuredId;
    });
    if (!persistent) {
      return activeStructuredId ? [activeStructuredId] : [];
    }
    return activeStructuredId ? [activeStructuredId, ...retained].slice(0, limit) : retained.slice(0, limit);
  }, [activeStructuredId, limit, persistent, recentIds, sessionsById]);

  if (visibleIds.length === 0) return null;

  return (
    <div className="relative flex-1 min-h-0 overflow-hidden">
      {visibleIds.map((sessionId) => {
        const session = sessionsById.get(sessionId);
        if (!session) return null;
        const active = visible && sessionId === activeSessionId;
        return (
          <div
            key={sessionId}
            aria-hidden={!active}
            inert={!active}
            className={
              active
                ? "absolute inset-0 flex flex-col min-h-0"
                : "absolute inset-0 flex flex-col min-h-0 invisible pointer-events-none"
            }
          >
            <StructuredView
              sessionId={sessionId}
              active={active}
              acpWorkerState={session.acp_worker_state ?? "absent"}
              tool={session.tool}
              acpAgent={session.acp_agent ?? null}
              clearAliases={session.clear_aliases}
              archivedAt={session.archived_at ?? null}
              snoozedUntil={session.snoozed_until ?? null}
              trashedAt={session.trashed_at ?? null}
              onRestore={session.trashed_at ? () => onRestoreSession?.(sessionId) : undefined}
              onOpenFileRef={onOpenFileRef}
              fileRefSession={session}
              onOpenAgentsPane={onOpenAgentsPane}
              isSandboxed={session.is_sandboxed}
            />
          </div>
        );
      })}
    </div>
  );
}

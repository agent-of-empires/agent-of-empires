import { useMemo, type ReactNode } from "react";
import { usePersistentSessionIds } from "../hooks/usePersistentSessionIds";
import {
  DEFAULT_PERSISTENT_STRUCTURED_VIEWS,
  normalizePersistentStructuredViewLimit,
} from "../lib/persistentStructuredViews";
import type { SessionResponse } from "../lib/types";

interface Props {
  /** False while the stack is covered (file viewer, other mobile pane). */
  active?: boolean;
  activeSessionId: string;
  sessions: SessionResponse[];
  persistent: boolean;
  maxPersistentViews?: number;
  renderSession: (session: SessionResponse, active: boolean) => ReactNode;
}

export function StructuredSessionStack({
  active = true,
  activeSessionId,
  sessions,
  persistent,
  maxPersistentViews = DEFAULT_PERSISTENT_STRUCTURED_VIEWS,
  renderSession,
}: Props) {
  const limit = normalizePersistentStructuredViewLimit(maxPersistentViews);
  const sessionsById = useMemo(() => new Map(sessions.map((session) => [session.id, session])), [sessions]);
  const visibleIds = usePersistentSessionIds(activeSessionId, sessionsById, persistent, limit);

  if (!sessionsById.has(activeSessionId)) return null;

  return (
    <div className="flex-1 min-h-0 overflow-hidden relative">
      {visibleIds.map((sessionId) => {
        const session = sessionsById.get(sessionId);
        if (!session) return null;
        const selected = sessionId === activeSessionId;
        return (
          <div
            key={sessionId}
            aria-hidden={!selected}
            inert={!selected}
            className={
              selected
                ? "absolute inset-0 flex flex-col min-h-0"
                : "absolute inset-0 flex flex-col min-h-0 invisible pointer-events-none"
            }
          >
            {renderSession(session, active && selected)}
          </div>
        );
      })}
    </div>
  );
}

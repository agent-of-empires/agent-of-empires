import { useEffect, useMemo, useState } from "react";
import type { SessionResponse } from "../lib/types";
import { DEFAULT_PERSISTENT_TERMINALS, normalizePersistentTerminalLimit } from "../lib/persistentTerminals";
import { TerminalView } from "./TerminalView";

interface Props {
  activeSessionId: string;
  sessions: SessionResponse[];
  persistent: boolean;
  maxPersistentTerminals?: number;
}

export function TerminalSessionStack({
  activeSessionId,
  sessions,
  persistent,
  maxPersistentTerminals = DEFAULT_PERSISTENT_TERMINALS,
}: Props) {
  const [recentIds, setRecentIds] = useState<string[]>([]);
  const limit = normalizePersistentTerminalLimit(maxPersistentTerminals);
  const sessionsById = useMemo(() => new Map(sessions.map((session) => [session.id, session])), [sessions]);
  const activeSession = sessionsById.get(activeSessionId);

  useEffect(() => {
    let cancelled = false;
    queueMicrotask(() => {
      if (cancelled) return;
      if (!persistent) {
        setRecentIds([]);
        return;
      }
      setRecentIds((ids) => {
        const inactiveLimit = Math.max(0, limit - 1);
        const inactive = ids
          .filter((id) => id !== activeSessionId)
          .filter((id) => sessionsById.has(id))
          .slice(0, inactiveLimit);
        const next = [activeSessionId, ...inactive];
        return next.join("\0") === ids.join("\0") ? ids : next;
      });
    });
    return () => {
      cancelled = true;
    };
  }, [activeSessionId, limit, persistent, sessionsById]);

  if (!activeSession) return null;

  const visibleIds = persistent
    ? [activeSessionId, ...recentIds.filter((id) => id !== activeSessionId)]
    : [activeSessionId];

  return (
    <div className="flex-1 min-h-0 overflow-hidden relative">
      {visibleIds.map((sessionId) => {
        const session = sessionsById.get(sessionId);
        if (!session) return null;
        const active = sessionId === activeSessionId;
        return (
          <div
            key={sessionId}
            aria-hidden={!active}
            className={
              active
                ? "absolute inset-0 flex flex-col min-h-0"
                : "absolute inset-0 flex flex-col min-h-0 invisible pointer-events-none"
            }
          >
            <TerminalView session={session} active={active} />
          </div>
        );
      })}
    </div>
  );
}

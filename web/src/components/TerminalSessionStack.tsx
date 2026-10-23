import { useMemo } from "react";
import { usePersistentSessionIds } from "../hooks/usePersistentSessionIds";
import type { SessionResponse } from "../lib/types";
import { DEFAULT_PERSISTENT_TERMINALS, normalizePersistentTerminalLimit } from "../lib/persistentTerminals";
import { TerminalView } from "./TerminalView";

interface Props {
  readOnly: boolean;
  active?: boolean;
  activeSessionId: string;
  sessions: SessionResponse[];
  persistent: boolean;
  maxPersistentTerminals?: number;
}

export function TerminalSessionStack({
  readOnly,
  active = true,
  activeSessionId,
  sessions,
  persistent,
  maxPersistentTerminals = DEFAULT_PERSISTENT_TERMINALS,
}: Props) {
  const limit = normalizePersistentTerminalLimit(maxPersistentTerminals);
  const sessionsById = useMemo(() => new Map(sessions.map((session) => [session.id, session])), [sessions]);
  const activeSession = sessionsById.get(activeSessionId);
  const visibleIds = usePersistentSessionIds(activeSessionId, sessionsById, persistent, limit);

  if (!activeSession) return null;

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
            className={
              selected
                ? "absolute inset-0 flex flex-col min-h-0"
                : "absolute inset-0 flex flex-col min-h-0 invisible pointer-events-none"
            }
          >
            <TerminalView session={session} readOnly={readOnly} active={active && selected} />
          </div>
        );
      })}
    </div>
  );
}

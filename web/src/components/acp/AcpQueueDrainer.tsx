// Headless owner for a structured-view session whose chat is not the one
// on screen but which still has queued prompts to deliver.
//
// Before #3331 `useAcpSession` existed only inside the visible chat, so a
// queued follow-up sat undelivered until the user navigated back to it.
// This mounts the same hook with no UI attached: it holds that session's
// WebSocket, watches for the turn to end, and lets the hook's own drain
// effect fire. Deliberately its own subscription rather than teaching the
// visible session's socket to schedule other sessions.
//
// The AgentProfileProvider is not decorative. The drain slices its batch
// at clear-command aliases (`/clear`, `/new`), and those aliases come from
// the profile. Without a provider carrying THIS session's tool the drain
// would read the default profile and batch a queued `/clear` into the
// combined prompt instead of firing it alone.

import { useMemo } from "react";

import { useBackgroundDrainSessionIds } from "../../hooks/useAcpQueueCount";
import { AgentProfileProvider } from "../../lib/agentProfileContext";
import { useAcpSession } from "../../hooks/useAcpSession";
import type { AcpWorkerState, SessionResponse } from "../../lib/types";

interface Props {
  sessionId: string;
  /** The session's `tool`, for clear-alias resolution. */
  tool: string | null | undefined;
  acpWorkerState: AcpWorkerState;
  archivedAt: string | null;
  snoozedUntil: string | null;
}

function DrainerSubscription({ sessionId, acpWorkerState, archivedAt, snoozedUntil }: Omit<Props, "tool">) {
  useAcpSession(sessionId, acpWorkerState, archivedAt, snoozedUntil);
  return null;
}

export function AcpQueueDrainer({ sessionId, tool, acpWorkerState, archivedAt, snoozedUntil }: Props) {
  return (
    <AgentProfileProvider toolKey={tool}>
      <DrainerSubscription
        sessionId={sessionId}
        acpWorkerState={acpWorkerState}
        archivedAt={archivedAt}
        snoozedUntil={snoozedUntil}
      />
    </AgentProfileProvider>
  );
}

/** Mount a drainer for every structured session that has queued prompts
 *  waiting and whose chat is not the one on screen. Renders nothing.
 *  Trashed sessions are skipped: their worker is down and only a restore
 *  brings it back, so a subscription would park forever. */
export function AcpBackgroundDrainers({
  sessions,
  activeSessionId,
}: {
  sessions: readonly SessionResponse[];
  activeSessionId: string | null;
}) {
  const candidates = useMemo(
    () => sessions.filter((s) => s.view === "structured" && s.id !== activeSessionId && !s.trashed_at),
    [sessions, activeSessionId],
  );
  const candidateIds = useMemo(() => candidates.map((s) => s.id), [candidates]);
  const drainIds = useBackgroundDrainSessionIds(candidateIds);
  return (
    <>
      {drainIds.map((id) => {
        const session = candidates.find((s) => s.id === id);
        if (!session) return null;
        return (
          <AcpQueueDrainer
            key={id}
            sessionId={id}
            tool={session.tool}
            acpWorkerState={session.acp_worker_state ?? "absent"}
            archivedAt={session.archived_at ?? null}
            snoozedUntil={session.snoozed_until ?? null}
          />
        );
      })}
    </>
  );
}

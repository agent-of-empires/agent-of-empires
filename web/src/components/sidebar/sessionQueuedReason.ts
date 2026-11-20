import { queuedReason } from "../acp/queuedReason";
import type { SessionResponse } from "../../lib/types";

/** The queued strip's reason line for a row that isn't open, from what the session list carries. */
export function sessionQueuedReason(session: SessionResponse, agent: string): string | null {
  const held = session.queue_reason;
  // The strip has no wording for a lost integration, so the badge shows the count alone.
  if (held === "integration_lost") return null;
  return queuedReason({
    agent,
    online: true,
    anyPending: false,
    waitingFor: null,
    rateLimitParked: session.rate_limit !== undefined,
    rateLimit: session.rate_limit ?? null,
    rateLimitAutoResume: session.rate_limit_auto_resume,
    workerDown: held === "worker_down",
    workerStarting: session.acp_worker_state === "resuming",
    cancelling: held === "cancelling",
    compacting: held === "compacting",
    // The Daemon names a turn only when it can't take the message, so it is never steerable here.
    turnActive: held === "turn_active",
    steerable: false,
    // Generic wording for a solo command too, so the badge never names the command.
    firstText: undefined,
    soloAliases: [],
  }).reason;
}

/** One reason for a row's sessions with queued messages, or null when they don't all give the same one. */
export function workspaceQueuedReason(
  sessions: readonly SessionResponse[],
  counts: readonly number[],
  agentName: (agent: string | null | undefined) => string,
): string | null {
  const reasons = sessions
    .filter((_, i) => (counts[i] ?? 0) > 0)
    .map((s) => sessionQueuedReason(s, agentName(s.acp_agent ?? s.tool)));
  const [first] = reasons;
  return first && reasons.every((r) => r === first) ? first : null;
}

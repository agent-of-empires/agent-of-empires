import type { AcpState } from "../../lib/acpTypes";
import type { SessionResponse } from "../../lib/types";
import type { AcpContext } from "./AcpRuntime";

type ConnectionRetryState = Pick<AcpContext, "status" | "reconnecting" | "retryCount" | "maxRetries">;

export function connectionRetriesExhausted({
  status,
  reconnecting,
  retryCount,
  maxRetries,
}: ConnectionRetryState): boolean {
  return status !== "open" && !reconnecting && retryCount >= maxRetries;
}

/** A Retry from the startup error banner is running; its startup line replaces the banner. */
export function isRetryingStart(state: Pick<AcpState, "restartInFlight" | "retriedError">): boolean {
  return state.restartInFlight && state.retriedError !== null;
}

/** The empty state while the Agent can't start, or until a Retry has started it. */
export function notRunningCopy(
  state: Pick<AcpState, "restartInFlight" | "retriedError" | "startupError">,
  name: string,
): string | null {
  const subject = name.charAt(0).toUpperCase() + name.slice(1);
  if (isRetryingStart(state)) return `${subject} isn't running yet.`;
  return state.startupError ? `${subject} isn't running.` : null;
}

/** The Agent's session ran before, so an older Session is known before its events load. An import has
 *  its id from creation but has never run here. */
export function hasStartedBefore(session: Pick<SessionResponse, "acp_session_id" | "import_pending">): boolean {
  return !!session.acp_session_id && !session.import_pending;
}

/** A new Session whose Agent session doesn't exist yet: its Worker, first connection, or
 *  `session/new` is still coming up. Events persisted before `session/new` answers
 *  (prompt capabilities, commands) don't end it; `AcpSessionAssigned` does.
 *  A Retry of a startup error counts too, for any Session. */
export function isNewSessionStarting(
  state: AcpState,
  connection: ConnectionRetryState & Pick<AcpContext, "hasEverOpened">,
  acpWorkerState: "absent" | "resuming" | "running" | "stopping",
  startedBefore: boolean,
): boolean {
  if (isRetryingStart(state)) return !connectionRetriesExhausted(connection);
  if (
    startedBefore ||
    state.sessionAssigned ||
    state.startupError ||
    state.workerStopped ||
    state.workerRestarting ||
    connectionRetriesExhausted(connection)
  )
    return false;
  const { status, hasEverOpened, reconnecting } = connection;
  if (acpWorkerState === "resuming") return true;
  // A running Worker's replay says whether its session was assigned, so wait for the connection.
  if (acpWorkerState === "running") return hasEverOpened;
  return !hasEverOpened && (reconnecting || status === "connecting" || status === "error");
}

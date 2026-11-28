import { nativeInputAvailable, type AcpState } from "../../lib/acpTypes";
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
  state: Pick<
    AcpState,
    | "restartInFlight"
    | "retriedError"
    | "startupError"
    | "connectionReady"
    | "nativeSnapshotSeq"
    | "connectionUnavailable"
  >,
  name: string,
): string | null {
  if (nativeInputAvailable(state) || state.connectionUnavailable !== null) return null;
  const subject = name.charAt(0).toUpperCase() + name.slice(1);
  if (isRetryingStart(state)) return `${subject} isn't running yet.`;
  return state.startupError ? `${subject} isn't running.` : null;
}

/** The Agent's session ran before, so an older Session is known before its events load. An import has
 *  its id from creation but has never run here. */
export function hasStartedBefore(session: Pick<SessionResponse, "acp_session_id" | "import_pending">): boolean {
  return !!session.acp_session_id && !session.import_pending;
}

/** Wait for current input authority, including a Retry on a previously started session. */
export function isNewSessionStarting(
  state: AcpState,
  connection: ConnectionRetryState & Pick<AcpContext, "hasEverOpened">,
  acpWorkerState: "absent" | "resuming" | "running" | "stopping",
  startedBefore: boolean,
): boolean {
  if (isRetryingStart(state)) return !connectionRetriesExhausted(connection);
  if (
    startedBefore ||
    nativeInputAvailable(state) ||
    state.startupError ||
    state.integrationLost ||
    state.workerStopped ||
    state.workerRestarting ||
    connectionRetriesExhausted(connection)
  )
    return false;
  const { status, hasEverOpened, reconnecting } = connection;
  if (acpWorkerState === "resuming") return true;
  // Wait for its current receipt after the connection opens.
  if (acpWorkerState === "running") return hasEverOpened;
  return !hasEverOpened && (reconnecting || status === "connecting" || status === "error");
}

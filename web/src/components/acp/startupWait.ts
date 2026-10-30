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

/** The Agent's session ran before, so an older Session is known before its events load. An import has
 *  its id from creation but has never run here. */
export function hasStartedBefore(session: Pick<SessionResponse, "acp_session_id" | "import_pending">): boolean {
  return !!session.acp_session_id && !session.import_pending;
}

/** A new Session (no events yet) whose Worker or first connection is still coming up. */
export function isNewSessionStarting(
  state: AcpState,
  connection: ConnectionRetryState & Pick<AcpContext, "hasEverOpened">,
  acpWorkerState: "absent" | "resuming" | "running" | "stopping",
  startedBefore: boolean,
): boolean {
  if (
    startedBefore ||
    state.startupError ||
    state.workerStopped ||
    state.workerRestarting ||
    state.lastSeq > 0 ||
    connectionRetriesExhausted(connection)
  )
    return false;
  const { status, hasEverOpened, reconnecting } = connection;
  // A running Worker is already up, so a cold connection to it is not a startup.
  return (
    acpWorkerState === "resuming" ||
    (acpWorkerState !== "running" && !hasEverOpened && (reconnecting || status === "connecting" || status === "error"))
  );
}

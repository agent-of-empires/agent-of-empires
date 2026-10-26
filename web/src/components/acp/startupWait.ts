import type { AcpState } from "../../lib/acpTypes";
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

/** A new Session (no events yet) whose Worker or first connection is still coming up. */
export function isNewSessionStarting(
  state: AcpState,
  connection: ConnectionRetryState & Pick<AcpContext, "hasEverOpened">,
  acpWorkerState: "absent" | "resuming" | "running" | "stopping",
): boolean {
  if (
    state.startupError ||
    state.workerStopped ||
    state.workerRestarting ||
    state.lastSeq > 0 ||
    connectionRetriesExhausted(connection)
  )
    return false;
  const { status, hasEverOpened, reconnecting } = connection;
  return (
    acpWorkerState === "resuming" || (!hasEverOpened && (reconnecting || status === "connecting" || status === "error"))
  );
}

import type { AcpState } from "../../lib/acpTypes";
/** A new Session (no events yet) whose Worker or first connection is still coming up. */
export function isNewSessionStarting(
  state: AcpState,
  status: string,
  hasEverOpened: boolean,
  acpWorkerState: "absent" | "resuming" | "running" | "stopping",
): boolean {
  if (state.startupError || state.workerStopped || state.workerRestarting || state.lastSeq > 0) return false;
  return acpWorkerState === "resuming" || (!hasEverOpened && (status === "connecting" || status === "error"));
}

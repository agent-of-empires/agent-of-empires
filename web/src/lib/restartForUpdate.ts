import { restartSessionForUpdate } from "./api";
import { toastBus } from "./toastBus";

export async function restartForUpdate(sessionId: string): Promise<void> {
  const result = await restartSessionForUpdate(sessionId);
  if (!result.ok) {
    toastBus.handler?.error(result.message ?? "Could not restart the session");
    return;
  }
  toastBus.handler?.info("Restarting with the updated agent. Your conversation is kept.");
}

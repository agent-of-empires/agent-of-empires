import { useCallback, useRef } from "react";

/** Force when the server confirmed a cancel or the user already pressed Stop this turn. */
export function nextCancelAction(cancelling: boolean, alreadyRequested: boolean): "cancel" | "force" {
  return cancelling || alreadyRequested ? "force" : "cancel";
}

/** Stop-button handler: a graceful cancel first, a force-end on the second press.
 *  The server only confirms cancels for prompts it has in flight, so an orphaned
 *  turn relies on the local intent, keyed by session and turn so it resets itself. */
export function useCancelEscalation(
  sessionId: string,
  turnSeq: number,
  cancelling: boolean,
  cancelPrompt: () => Promise<void>,
  forceEndTurn: () => Promise<void>,
): () => Promise<void> {
  const requestedAtRef = useRef<string | null>(null);

  return useCallback(async () => {
    const token = `${sessionId}:${turnSeq}`;
    const alreadyRequested = requestedAtRef.current === token;
    if (nextCancelAction(cancelling, alreadyRequested) === "force") {
      await forceEndTurn();
    } else {
      requestedAtRef.current = token;
      await cancelPrompt();
    }
  }, [sessionId, turnSeq, cancelling, cancelPrompt, forceEndTurn]);
}

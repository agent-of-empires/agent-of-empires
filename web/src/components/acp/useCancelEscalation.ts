import { useCallback, useRef } from "react";

/**
 * Decides whether a Stop-button press should send a graceful cancel or
 * escalate to a force-end. Pure so it can be unit-tested without React.
 * Escalation happens when the server has confirmed a cancel is in flight
 * (`cancelling`) OR the user has already pressed Stop once this turn
 * (`alreadyRequested`), so the escape hatch never depends on an
 * acknowledgement from the wedged daemon. See #2237.
 */
export function nextCancelAction(cancelling: boolean, alreadyRequested: boolean): "cancel" | "force" {
  return cancelling || alreadyRequested ? "force" : "cancel";
}

/**
 * Owns the Stop-button escalation state for the structured-view composer and
 * returns the `onCancel` handler.
 *
 * The server confirms a cancel by flipping `cancelling` via a
 * `CancelRequested` event, but that event is only emitted while a prompt is
 * in flight on the daemon. When the UI is stuck "active" with no in-flight
 * prompt (an adopted/orphaned turn whose terminal `Stopped` was lost, see
 * #1216), `CancelRequested` never arrives, so `cancelling` never flips and the
 * Stop button could never escalate. We track a client-owned "already pressed
 * Stop this turn" intent so a second press always force-ends, independent of
 * any acknowledgement from the system it is escaping. See #2237.
 *
 * The intent is captured as a `(sessionId, turnSeq)` token rather than reset
 * via an effect: a new turn bumps `turnSeq` and a session switch changes
 * `sessionId`, so either one is automatically a mismatch and the first Stop of
 * the next turn or session is graceful again. `turnSeq` is the monotonic
 * per-turn prompt counter (`pendingUserPromptSeq`).
 */
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
    // First Stop sends a graceful cancel; a second escalates to force-end
    // instead of resending a no-op notification, so the user's instinct to
    // click again actually ends the turn. See #1727 / #2237.
    if (nextCancelAction(cancelling, alreadyRequested) === "force") {
      await forceEndTurn();
    } else {
      requestedAtRef.current = token;
      await cancelPrompt();
    }
  }, [sessionId, turnSeq, cancelling, cancelPrompt, forceEndTurn]);
}

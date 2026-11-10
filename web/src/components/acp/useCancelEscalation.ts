import { useCallback, useLayoutEffect, useRef, useState } from "react";

/** Force when the server confirmed cancellation or a graceful cancel was accepted this turn. */
export function nextCancelAction(cancelling: boolean, alreadyRequested: boolean): "cancel" | "force" {
  return cancelling || alreadyRequested ? "force" : "cancel";
}

type CancelIdentity = { sessionId: string; turnSeq: number };

/** Refused gestures never arm local force-end; admission belongs to one session/turn. */
export function useCancelEscalation(
  sessionId: string,
  turnSeq: number,
  cancelling: boolean,
  cancelPrompt: () => Promise<boolean>,
  forceEndTurn: () => Promise<void>,
): { onCancel: () => Promise<void>; canForceEndTurn: boolean } {
  const [identity, setIdentity] = useState<CancelIdentity>(() => ({ sessionId, turnSeq }));
  if (identity.sessionId !== sessionId || identity.turnSeq !== turnSeq) {
    setIdentity({ sessionId, turnSeq });
  }
  const committed = useRef<{ identity: CancelIdentity; cancelling: boolean } | null>(null);
  useLayoutEffect(() => {
    committed.current = { identity, cancelling };
    return () => {
      committed.current = null;
    };
  }, [identity, cancelling]);
  const requestedAtRef = useRef<CancelIdentity | null>(null);
  const [requestedAt, setRequestedAt] = useState<CancelIdentity | null>(null);
  const inFlight = useRef<{ identity: CancelIdentity; promise: Promise<void> } | null>(null);

  const onCancel = useCallback(async () => {
    const current = committed.current;
    if (current?.identity !== identity) return;
    if (nextCancelAction(current.cancelling, requestedAtRef.current === identity) === "force") {
      await forceEndTurn();
      return;
    }
    if (inFlight.current?.identity === identity) {
      await inFlight.current.promise;
      return;
    }
    const request: { identity: CancelIdentity; promise: Promise<void> } = {
      identity,
      promise: cancelPrompt()
        .then((accepted) => {
          if (accepted && committed.current?.identity === identity) {
            requestedAtRef.current = identity;
            setRequestedAt(identity);
          }
        })
        .finally(() => {
          if (inFlight.current === request) inFlight.current = null;
        }),
    };
    inFlight.current = request;
    await request.promise;
  }, [identity, cancelPrompt, forceEndTurn]);
  return { onCancel, canForceEndTurn: cancelling || requestedAt === identity };
}

export interface MobileKeyboardProxyInput {
  inputType: string;
  data: string | null;
  isComposing: boolean;
}

type Receiver = (input: MobileKeyboardProxyInput) => void;

const MAX_PENDING_INPUTS = 128;
let receiver: Receiver | null = null;
let pending: MobileKeyboardProxyInput[] = [];

/** Send a semantic soft-keyboard edit to the active terminal, or retain it
 * briefly while a newly selected session is still mounting. */
export function deliverMobileKeyboardProxyInput(input: MobileKeyboardProxyInput) {
  if (receiver) {
    receiver(input);
    return;
  }
  if (pending.length < MAX_PENDING_INPUTS) pending.push(input);
}

/** Make one live terminal the receiver for the persistent iOS keyboard. */
export function registerMobileKeyboardProxyReceiver(next: Receiver) {
  receiver = next;
  const queued = pending;
  pending = [];
  for (const input of queued) next(input);
  return () => {
    if (receiver === next) receiver = null;
  };
}

/** A session change must never send its old keystrokes to the next session. */
export function clearMobileKeyboardProxyInput() {
  receiver = null;
  pending = [];
}

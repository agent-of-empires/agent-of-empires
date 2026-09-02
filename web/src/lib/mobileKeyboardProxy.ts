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

/** Translate a native `beforeinput` on a hidden terminal textarea into a
 * semantic soft-keyboard edit.
 *
 * `insertText` and `deleteContentBackward` are forwarded AND left to mutate
 * the textarea. iOS WebKit fires no composition events for the Korean
 * keyboard (WebKit bug 274700): every keystroke rewrites the trailing
 * syllable as `deleteContentBackward` + `insertText` ("ㅎ" -> "하" -> "한").
 * WebKit dispatches no `beforeinput` for a delete with nothing before the
 * caret, so with an always-empty textarea the deletes vanish and the PTY
 * receives every intermediate syllable ("ㅎ하한"). Keeping the typed text in
 * the textarea is what makes those deletes observable. See #1450 / #1615 for
 * the soft-keyboard Backspace path this shares.
 *
 * Line breaks and pastes never reach the textarea. A line break is also the
 * safe point to drop the accumulated text: no IME re-edits a syllable across
 * Enter. */
export function forwardTerminalBeforeInput(ev: InputEvent, deliver: (input: MobileKeyboardProxyInput) => void) {
  switch (ev.inputType) {
    case "insertText":
    case "deleteContentBackward":
      deliver({ inputType: ev.inputType, data: ev.data, isComposing: ev.isComposing });
      break;
    case "insertLineBreak":
    case "insertParagraph":
      ev.preventDefault();
      deliver({ inputType: ev.inputType, data: ev.data, isComposing: ev.isComposing });
      if (ev.target instanceof HTMLTextAreaElement) ev.target.value = "";
      break;
    case "insertFromPaste":
      ev.preventDefault();
      deliver({ inputType: ev.inputType, data: ev.data, isComposing: ev.isComposing });
      break;
    default:
      break;
  }
}

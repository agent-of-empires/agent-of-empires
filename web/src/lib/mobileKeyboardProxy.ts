export interface MobileKeyboardProxyInput {
  inputType: string;
  data: string | null;
  isComposing: boolean;
}

/** Returns whether the edit reached the pane, so the caller can decide
 *  whether the shadow textarea may record it. */
type Receiver = (input: MobileKeyboardProxyInput) => boolean;

const MAX_PENDING_INPUTS = 128;
let receiver: Receiver | null = null;
let pending: MobileKeyboardProxyInput[] = [];

/** Send a semantic soft-keyboard edit to the active terminal, or retain it
 * briefly while a newly selected session is still mounting. */
export function deliverMobileKeyboardProxyInput(input: MobileKeyboardProxyInput): boolean {
  if (receiver) return receiver(input);
  if (pending.length >= MAX_PENDING_INPUTS) return false;
  pending.push(input);
  return true;
}

/** Make one live terminal the receiver for the persistent iOS keyboard. */
export function registerMobileKeyboardProxyReceiver(next: Receiver) {
  receiver = next;
  const queued = pending;
  pending = [];
  let retained: string | null = null;
  for (const input of queued) {
    const accepted = next(input);
    if (
      !accepted ||
      input.inputType === "insertLineBreak" ||
      input.inputType === "insertParagraph" ||
      input.inputType === "insertFromPaste"
    ) {
      retained = "";
    } else if (retained !== null) {
      if (input.inputType === "insertText") retained += input.data ?? "";
      else if (input.inputType === "deleteContentBackward") retained = Array.from(retained).slice(0, -1).join("");
    }
  }
  // Browser edits were already applied while buffered. Rebuild only the
  // accepted suffix after an invalidation, without discarding later input.
  if (retained !== null) {
    const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]");
    if (proxy) proxy.value = retained;
  }
  return () => {
    if (receiver === next) receiver = null;
  };
}

/** A session change must never send its old keystrokes to the next session. */
export function clearMobileKeyboardProxyInput() {
  receiver = null;
  pending = [];
}

/** Out-of-band terminal input (toolbar keys, paste, hardware chords) reaches
 * the PTY without a `beforeinput`, so a retained syllable stops mirroring the
 * line it was a shadow of. Drop it, or the next Korean keystroke rewrites the
 * stale value as `DEL + replacement` into whatever the PTY now shows.
 *
 * Both hidden inputs are cleared: either the live terminal's own textarea or
 * App's persistent proxy can hold focus, and a caller reacting to a click has
 * no event target to tell it which. */
export function invalidateRetainedImeContext(target?: HTMLTextAreaElement | null) {
  if (target) target.value = "";
  const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]");
  if (proxy) proxy.value = "";
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
 * An edit the pane refused (a Ctrl chord that becomes a control code, a
 * read-only viewer's dropped keystroke) must not stay in the textarea
 * either: the shadow would then hold text the PTY never received, and the
 * next rewrite's delete would eat a character the user did not type.
 *
 * Line breaks and pastes never reach the textarea. A line break is also the
 * safe point to drop the accumulated text: no IME re-edits a syllable across
 * Enter. */
export function forwardTerminalBeforeInput(ev: InputEvent, deliver: Receiver) {
  switch (ev.inputType) {
    case "insertText":
    case "deleteContentBackward":
      if (!deliver({ inputType: ev.inputType, data: ev.data, isComposing: ev.isComposing })) {
        if (ev.target instanceof HTMLTextAreaElement) ev.target.value = "";
        ev.preventDefault();
      }
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

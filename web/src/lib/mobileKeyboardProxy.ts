export interface MobileKeyboardProxyInput {
  inputType: string;
  data: string | null;
  isComposing: boolean;
}

/** Whether the edit reached the pane, so the shadow textarea may record it. */
type Receiver = (input: MobileKeyboardProxyInput) => boolean;

const MAX_PENDING_INPUTS = 128;
let receiver: Receiver | null = null;
let pending: MobileKeyboardProxyInput[] = [];

/** Retains the edit briefly while a newly selected session is mounting. */
export function deliverMobileKeyboardProxyInput(input: MobileKeyboardProxyInput): boolean {
  if (receiver) return receiver(input);
  if (pending.length >= MAX_PENDING_INPUTS) return false;
  pending.push(input);
  return true;
}

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
  // Buffered edits were already applied; rebuild only the accepted suffix.
  if (retained !== null) {
    const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]");
    if (proxy) proxy.value = retained;
  }
  return () => {
    if (receiver === next) receiver = null;
  };
}

/** A session change must never send old keystrokes to the next session. */
export function clearMobileKeyboardProxyInput() {
  receiver = null;
  pending = [];
}

/** Out-of-band input desyncs a retained IME syllable, so clear both hidden inputs (either may hold focus). */
export function invalidateRetainedImeContext(target?: HTMLTextAreaElement | null) {
  if (target) target.value = "";
  const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]");
  if (proxy) proxy.value = "";
}

/** Forward `beforeinput` as a semantic edit. Inserts and deletes also mutate the textarea: iOS Korean input (WebKit bug 274700) rewrites syllables via deletes that only fire when text precedes the caret. Refused edits and line breaks clear the shadow text. */
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

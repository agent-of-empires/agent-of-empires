import { HIDDEN_INPUT_SENTINEL, stepHiddenInput } from "./hiddenInputDiff";
import { traceSent } from "./inputTrace";

/** A synced state diff, a line break, or a paste, as the hidden inputs report them to the terminal. */
export type MobileKeyboardProxyInput =
  | { inputType: "edit"; deleted: number; data: string }
  | { inputType: "insertLineBreak" | "insertParagraph" }
  | { inputType: "insertFromPaste"; data: string | null };

/** Whether the input reached the pane; a refused edit resets the textarea so it never shadows unsent text. */
type Receiver = (input: MobileKeyboardProxyInput) => boolean;

const PROXY_SELECTOR = "[data-keyboard-proxy]";
const MAX_PENDING_INPUTS = 128;
let receiver: Receiver | null = null;
let pending: MobileKeyboardProxyInput[] = [];

/** What each hidden input's pane is believed to hold from it, sentinel included. */
const baselines = new WeakMap<HTMLTextAreaElement, string>();

/** Drops the textarea's tracked text; the pane keeps it, but later edits no longer rewrite it. */
export function resetHiddenInput(ta: HTMLTextAreaElement) {
  ta.value = HIDDEN_INPUT_SENTINEL;
  baselines.set(ta, HIDDEN_INPUT_SENTINEL);
}

/** Sends the textarea's change since the last sync. */
function syncHiddenInput(ta: HTMLTextAreaElement, deliver: Receiver, label: string) {
  const step = stepHiddenInput(baselines.get(ta) ?? HIDDEN_INPUT_SENTINEL, ta.value);
  if (step.refill) {
    ta.setRangeText(step.refill, 0, 0);
    ta.setSelectionRange(ta.value.length, ta.value.length);
  }
  baselines.set(ta, ta.value);
  if (step.deleted === 0 && !step.inserted) return;
  traceSent(label, step.deleted, step.inserted);
  if (!deliver({ inputType: "edit", deleted: step.deleted, data: step.inserted })) resetHiddenInput(ta);
}

/**
 * Forwards a hidden textarea's edits by diffing its value against the baseline on `input`, and at
 * `compositionend` for a real IME composition (iOS Korean fires none, WebKit bug 274700, and syncs per edit).
 * Line breaks and pastes are cancelled and reported as such.
 */
export function bindHiddenInput(ta: HTMLTextAreaElement, deliver: Receiver, label: string): () => void {
  if (!baselines.has(ta)) resetHiddenInput(ta);
  let composing = false;
  const sync = () => syncHiddenInput(ta, deliver, label);
  const onBeforeInput = (ev: InputEvent) => {
    switch (ev.inputType) {
      case "insertLineBreak":
      case "insertParagraph":
        ev.preventDefault();
        sync();
        deliver({ inputType: ev.inputType });
        resetHiddenInput(ta);
        break;
      case "insertFromPaste":
        ev.preventDefault();
        deliver({ inputType: ev.inputType, data: ev.data });
        break;
    }
  };
  const onInput = (ev: Event) => {
    if (!composing && !(ev as InputEvent).isComposing) sync();
  };
  // The pane cursor stays at the end of the tracked text, so a caret moved away (the iOS spacebar trackpad)
  // returns there before the next key edits. Edits landing elsewhere still diff correctly, by retyping the tail.
  // Chords are skipped: moving the caret would clear the page selection Ctrl+Shift+C copies.
  const onKeyDown = (ev: KeyboardEvent) => {
    const end = ta.value.length;
    if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
    if (composing || ev.isComposing || ta.selectionStart !== ta.selectionEnd || ta.selectionEnd === end) return;
    ta.setSelectionRange(end, end);
  };
  const onCompositionStart = () => {
    composing = true;
  };
  const onCompositionEnd = () => {
    composing = false;
    sync();
  };
  const listeners: [string, EventListener][] = [
    ["beforeinput", onBeforeInput as EventListener],
    ["input", onInput],
    ["keydown", onKeyDown as EventListener],
    ["compositionstart", onCompositionStart],
    ["compositionend", onCompositionEnd],
  ];
  for (const [type, fn] of listeners) ta.addEventListener(type, fn);
  return () => {
    for (const [type, fn] of listeners) ta.removeEventListener(type, fn);
  };
}

/** Retains the input briefly while a newly selected session is mounting. */
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
  let refused = false;
  for (const input of queued) if (!next(input)) refused = true;
  // The proxy already applied the queued edits, so one the pane refused leaves it shadowing unsent text.
  const proxy = document.querySelector<HTMLTextAreaElement>(PROXY_SELECTOR);
  if (refused && proxy) resetHiddenInput(proxy);
  return () => {
    if (receiver === next) receiver = null;
  };
}

/** A session change must never send old keystrokes to the next session. */
export function clearMobileKeyboardProxyInput() {
  receiver = null;
  pending = [];
  const proxy = document.querySelector<HTMLTextAreaElement>(PROXY_SELECTOR);
  if (proxy) resetHiddenInput(proxy);
}

/** Out-of-band input changed the pane line, so reset both hidden inputs' baselines (either may hold focus). */
export function invalidateRetainedImeContext(target?: HTMLTextAreaElement | null) {
  if (target) resetHiddenInput(target);
  const proxy = document.querySelector<HTMLTextAreaElement>(PROXY_SELECTOR);
  if (proxy) resetHiddenInput(proxy);
}

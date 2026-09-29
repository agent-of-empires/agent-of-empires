// `?livedebug=1` input trace for diagnosing soft-keyboard, IME, and dictation event streams on a device.

const MAX_ENTRIES = 200;
const VALUE_TAIL = 24;
const TRACED_EVENTS = [
  "keydown",
  "beforeinput",
  "input",
  "compositionstart",
  "compositionupdate",
  "compositionend",
  "paste",
] as const;

let enabled = false;
let entries: string[] = [];
const listeners = new Set<() => void>();

const show = (s: string | null | undefined) => (s == null ? "-" : JSON.stringify(s));

function push(line: string) {
  entries = [...entries.slice(-(MAX_ENTRIES - 1)), `${performance.now().toFixed(0)} ${line}`];
  for (const notify of listeners) notify();
}

function formatEvent(label: string, e: Event): string {
  const ta = e.target as HTMLTextAreaElement;
  const parts = [label, e.type];
  if (e instanceof InputEvent) parts.push(e.inputType, show(e.data), e.isComposing ? "comp" : "");
  else if (e instanceof KeyboardEvent) parts.push(show(e.key), e.isComposing ? "comp" : "");
  else if (e instanceof CompositionEvent) parts.push(show(e.data));
  const value = ta.value;
  const tail = value.length > VALUE_TAIL ? `…${value.slice(-VALUE_TAIL)}` : value;
  parts.push(`sel=${ta.selectionStart},${ta.selectionEnd}/${value.length}`, `v=${show(tail)}`);
  return parts.filter(Boolean).join(" ");
}

/** Records the events on `ta`, in the capture phase so they are seen before any handler edits the value. */
export function traceInputEvents(ta: HTMLTextAreaElement, label: string): () => void {
  enabled = true;
  const record = (e: Event) => push(formatEvent(label, e));
  for (const type of TRACED_EVENTS) ta.addEventListener(type, record, true);
  return () => {
    for (const type of TRACED_EVENTS) ta.removeEventListener(type, record, true);
  };
}

/** Records what a hidden input sent to the pane; a no-op until a trace is attached. */
export function traceSent(label: string, deleted: number, inserted: string) {
  if (enabled) push(`${label} SEND del=${deleted} ${show(inserted)}`);
}

export function subscribeInputTrace(notify: () => void) {
  listeners.add(notify);
  return () => {
    listeners.delete(notify);
  };
}

export const inputTraceSnapshot = () => entries;

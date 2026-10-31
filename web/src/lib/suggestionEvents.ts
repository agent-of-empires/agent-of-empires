const EVENT = "aoe:suggestions-changed";
const target = new EventTarget();

/** Tells every mounted suggestions view to reload, so a change made in one shows in the others. */
export function notifySuggestionsChanged(): void {
  target.dispatchEvent(new Event(EVENT));
}

export function onSuggestionsChanged(listener: () => void): () => void {
  target.addEventListener(EVENT, listener);
  return () => target.removeEventListener(EVENT, listener);
}

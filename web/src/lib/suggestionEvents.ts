const EVENT = "aoe:suggestions-changed";

/** Tells every mounted suggestions view to reload, so a change made in one shows in the others. */
export function notifySuggestionsChanged(): void {
  window.dispatchEvent(new Event(EVENT));
}

export function onSuggestionsChanged(listener: () => void): () => void {
  window.addEventListener(EVENT, listener);
  return () => window.removeEventListener(EVENT, listener);
}

/** Fired after a settings save lands, so every reader re-reads rather than
 *  keeping the value it loaded. */
export const SETTINGS_CHANGED_EVENT = "aoe:settings-changed";

export function notifySettingsChanged(): void {
  window.dispatchEvent(new Event(SETTINGS_CHANGED_EVENT));
}

export function onSettingsChanged(listener: () => void): () => void {
  window.addEventListener(SETTINGS_CHANGED_EVENT, listener);
  return () => window.removeEventListener(SETTINGS_CHANGED_EVENT, listener);
}

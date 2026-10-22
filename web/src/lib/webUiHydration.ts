// Whether this page load's web UI state hydration has settled, in a module of its own so the
// synced keys' owners can wait for it without importing webUiSync (which imports them).

const HYDRATED_EVENT = "aoe:web-ui-state-hydrated";
let hydrated = false;

export function markWebUiStateHydrated(): void {
  hydrated = true;
  window.dispatchEvent(new Event(HYDRATED_EVENT));
}

/** Whether this page load's hydration has settled, successfully or not. */
export function isWebUiStateHydrated(): boolean {
  return hydrated;
}

/** Hydration isn't awaited before render, so a view that read its keys on mount re-reads here. */
export function onWebUiStateHydrated(listener: () => void): () => void {
  window.addEventListener(HYDRATED_EVENT, listener);
  return () => window.removeEventListener(HYDRATED_EVENT, listener);
}

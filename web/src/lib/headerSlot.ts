import { useSyncExternalStore } from "react";

// Where a full page such as Settings renders its own controls, in the middle of the app header.
let slot: HTMLElement | null = null;
const listeners = new Set<() => void>();

/** The header's ref callback for its page slot. */
export function setHeaderPageSlot(el: HTMLElement | null): void {
  if (slot === el) return;
  slot = el;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** The header's page slot while the header shows one, for a portal. */
export function useHeaderPageSlot(): HTMLElement | null {
  return useSyncExternalStore(
    subscribe,
    () => slot,
    () => null,
  );
}

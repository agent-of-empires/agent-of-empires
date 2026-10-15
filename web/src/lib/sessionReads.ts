import { useSyncExternalStore } from "react";

// Poll ordinals: a read is trusted as post-save truth only if it started after the save was acknowledged.
let started = 0;
let applied = 0;
const listeners = new Set<() => void>();

export const beginRead = () => ++started;
export const startedReads = () => started;

export function markReadApplied(n: number) {
  applied = n;
  listeners.forEach((l) => l());
}

/** Ordinal of the session read currently on screen; 0 and no subscription while inactive. */
export function useAppliedRead(active: boolean) {
  return useSyncExternalStore(
    (l) => {
      if (!active) return () => {};
      listeners.add(l);
      return () => void listeners.delete(l);
    },
    () => (active ? applied : 0),
  );
}

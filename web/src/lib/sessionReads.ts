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

/** Ordinal of the session read currently on screen. */
export function useAppliedRead() {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => void listeners.delete(l);
    },
    () => applied,
  );
}

import { useCallback, useRef, useSyncExternalStore } from "react";

export interface SnapshotStore<T> {
  state: T;
  /** The committed snapshot, for callbacks that must not close over a stale render. */
  read: () => T;
  setState: (next: (prev: T) => T) => void;
}

/** An external store behind `useSyncExternalStore`, so a reader sees every update in order. */
export function useSnapshotStore<T>(initial: () => T): SnapshotStore<T> {
  const storeRef = useRef<{ snapshot: T; listeners: Set<() => void> } | null>(null);
  storeRef.current ??= { snapshot: initial(), listeners: new Set() };

  const setState = useCallback((next: (prev: T) => T) => {
    const store = storeRef.current!;
    store.snapshot = next(store.snapshot);
    store.listeners.forEach((l) => l());
  }, []);
  const subscribe = useCallback((listener: () => void) => {
    storeRef.current!.listeners.add(listener);
    return () => void storeRef.current!.listeners.delete(listener);
  }, []);
  const read = useCallback(() => storeRef.current!.snapshot, []);

  return { state: useSyncExternalStore(subscribe, read), read, setState };
}

/**
 * A saved value shown until the refetch it triggers lands. `inflight` counts unfinished PATCHes;
 * `acked` is the last value the server accepted, which a later rejected save falls back to.
 */
export type Pending = Record<
  string,
  { section: string; field: string; value: unknown; inflight: number; acked?: { value: unknown } }
>;

// Module-level, so a remount keeps the saves still in flight instead of rebuilding from a stale value.
let pendingSaves: Pending = {};
let queue: Record<string, Promise<void>> = {};
const listeners = new Set<() => void>();

export function getPendingSaves() {
  return pendingSaves;
}

export function setPending(update: (p: Pending) => Pending) {
  pendingSaves = update(pendingSaves);
  for (const l of listeners) l();
}

export function subscribePendingSaves(cb: () => void) {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

/** The server applies each PATCH atomically but not in arrival order, so send a field's saves one at a time. */
export function queueSave<T>(key: string, send: () => Promise<T>): Promise<T> {
  const request = (queue[key] ?? Promise.resolve()).then(send);
  queue[key] = request.then(
    () => undefined,
    () => undefined,
  );
  return request;
}

export function resetPluginSaves() {
  pendingSaves = {};
  queue = {};
}

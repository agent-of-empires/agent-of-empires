import type { ServerQueuedPrompt } from "../../lib/api";

export type QueueMutation =
  | { kind: "edit"; id: string; text: string }
  | { kind: "remove" | "send"; id: string }
  | { kind: "clear"; ids: string[] };

type QueueSync = {
  revision: number;
  pending: Map<symbol, QueueMutation>;
  enqueues: Map<string, Promise<ServerQueuedPrompt | null>>;
  latestEdit: Map<string, symbol>;
  listeners: Set<() => void>;
};

const sessions = new Map<string, QueueSync>();

function sessionQueue(id: string): QueueSync {
  let sync = sessions.get(id);
  if (!sync) {
    sync = { revision: 0, pending: new Map(), enqueues: new Map(), latestEdit: new Map(), listeners: new Set() };
    sessions.set(id, sync);
  }
  return sync;
}

function changed(sync: QueueSync) {
  sync.revision += 1;
  for (const listener of sync.listeners) listener();
}

/** Share settlement with retained views and newly mounted hooks for the same session. */
export function subscribeServerQueue(id: string, listener: () => void): () => void {
  const sync = sessionQueue(id);
  sync.listeners.add(listener);
  return () => {
    sync.listeners.delete(listener);
    if (sync.listeners.size === 0 && sync.pending.size === 0 && sync.enqueues.size === 0) sessions.delete(id);
  };
}

export function serverQueueRevision(id: string): number {
  return sessionQueue(id).revision;
}

/** An enqueue confirmation invalidates snapshots started before that server write. */
export function confirmServerQueue(id: string) {
  const sync = sessions.get(id);
  if (sync) changed(sync);
}

/** Keep optimistic mutations over fresh server rows without blocking unrelated retirement. */
export function reconcileQueueSnapshot(
  id: string,
  revision: number,
  incoming: ServerQueuedPrompt[],
): ServerQueuedPrompt[] | null {
  const sync = sessions.get(id);
  if (!sync || revision !== sync.revision) return null;
  let rows = incoming;
  for (const [token, mutation] of sync.pending) {
    if (mutation.kind === "clear") rows = rows.filter((row) => !mutation.ids.includes(row.id));
    else if (mutation.kind === "edit") {
      if (sync.latestEdit.get(mutation.id) !== token) continue;
      rows = rows.map((row) => {
        if (row.id === mutation.id) return { ...row, text: mutation.text };
        return row;
      });
    } else rows = rows.filter((row) => row.id !== mutation.id);
  }
  return rows;
}

/** Retain enqueue ordering across remounts so a later removal cannot run ahead of its POST. */
export async function trackServerQueueEnqueue(
  id: string,
  rowId: string,
  request: () => Promise<ServerQueuedPrompt | null>,
): Promise<ServerQueuedPrompt | null> {
  const sync = sessionQueue(id);
  const previous = sync.enqueues.get(rowId);
  if (previous) return previous;
  const pending = request();
  sync.enqueues.set(rowId, pending);
  try {
    return await pending;
  } finally {
    if (sync.enqueues.get(rowId) === pending) sync.enqueues.delete(rowId);
    if (sync.listeners.size === 0 && sync.pending.size === 0 && sync.enqueues.size === 0) sessions.delete(id);
  }
}

/** Reserve an optimistic overlay until the request settles, even across unmounts. */
export async function mutateServerQueue<T>(id: string, mutation: QueueMutation, request: () => Promise<T>): Promise<T> {
  const sync = sessionQueue(id);
  const token = Symbol();
  sync.pending.set(token, mutation);
  if (mutation.kind === "edit") sync.latestEdit.set(mutation.id, token);
  changed(sync);
  let ids: string[];
  if (mutation.kind === "clear") ids = mutation.ids;
  else ids = [mutation.id];
  const enqueues = ids.map((rowId) => sync.enqueues.get(rowId)).filter((pending) => pending !== undefined);
  try {
    if (enqueues.length > 0) await Promise.allSettled(enqueues);
    return await request();
  } finally {
    sync.pending.delete(token);
    if (mutation.kind === "edit" && sync.latestEdit.get(mutation.id) === token) sync.latestEdit.delete(mutation.id);
    changed(sync);
    if (sync.listeners.size === 0 && sync.pending.size === 0 && sync.enqueues.size === 0) sessions.delete(id);
  }
}

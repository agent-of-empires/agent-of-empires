import type { ServerQueuedPrompt } from "../../lib/api";

export type QueueMutation =
  | { kind: "edit"; id: string; text: string }
  | { kind: "remove" | "send"; id: string }
  | { kind: "clear" };

type QueueSync = {
  revision: number;
  pending: Map<symbol, QueueMutation>;
  listeners: Set<() => void>;
};

const sessions = new Map<string, QueueSync>();

function sessionQueue(id: string): QueueSync {
  let sync = sessions.get(id);
  if (!sync) {
    sync = { revision: 0, pending: new Map(), listeners: new Set() };
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
    if (sync.listeners.size === 0 && sync.pending.size === 0) sessions.delete(id);
  };
}

export function serverQueueRevision(id: string): number {
  return sessionQueue(id).revision;
}

/** Keep optimistic mutations over fresh server rows without blocking unrelated retirement. */
export function reconcileQueueSnapshot(
  id: string,
  revision: number,
  incoming: ServerQueuedPrompt[],
): ServerQueuedPrompt[] | null {
  const sync = sessionQueue(id);
  if (revision !== sync.revision) return null;
  let rows = incoming;
  for (const mutation of sync.pending.values()) {
    if (mutation.kind === "clear") rows = [];
    else if (mutation.kind === "edit") {
      rows = rows.map((row) => {
        if (row.id === mutation.id) return { ...row, text: mutation.text };
        return row;
      });
    } else rows = rows.filter((row) => row.id !== mutation.id);
  }
  return rows;
}

/** Reserve an optimistic overlay until the request settles, even across unmounts. */
export async function mutateServerQueue<T>(id: string, mutation: QueueMutation, request: () => Promise<T>): Promise<T> {
  const sync = sessionQueue(id);
  const token = Symbol();
  sync.pending.set(token, mutation);
  changed(sync);
  try {
    return await request();
  } finally {
    sync.pending.delete(token);
    changed(sync);
    if (sync.listeners.size === 0 && sync.pending.size === 0) sessions.delete(id);
  }
}

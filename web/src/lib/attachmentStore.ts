// Staged composer attachments in IndexedDB, one record per session id. localStorage's ~5 MB
// quota cannot hold a photo's base64, IndexedDB's can. Every call rejects when IndexedDB is
// unavailable or the write fails, so callers decide how to degrade.

import type { PromptAttachmentInput } from "./acpTypes";

const DB_NAME = "aoe-composer-attachments";
const STORE = "drafts";

let dbPromise: Promise<IDBDatabase> | null = null;

function openDb(): Promise<IDBDatabase> {
  if (dbPromise) return dbPromise;
  const opened = new Promise<IDBDatabase>((resolve, reject) => {
    if (typeof indexedDB === "undefined") {
      reject(new Error("indexedDB unavailable"));
      return;
    }
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE);
    req.onsuccess = () => {
      const db = req.result;
      // Another tab upgrading or the browser reclaiming storage closes us; reopen on next use.
      db.onclose = () => (dbPromise = null);
      db.onversionchange = () => {
        // An explicit close() fires no `close` event, so drop the cached handle here too.
        dbPromise = null;
        db.close();
      };
      resolve(db);
    };
    req.onerror = () => reject(req.error ?? new Error("indexedDB open failed"));
    req.onblocked = () => reject(new Error("indexedDB open blocked"));
  });
  dbPromise = opened;
  // A failed open must not be cached, so a later call can retry.
  opened.catch(() => {
    if (dbPromise === opened) dbPromise = null;
  });
  return opened;
}

async function run<T>(mode: IDBTransactionMode, fn: (store: IDBObjectStore) => IDBRequest<T>): Promise<T> {
  const db = await openDb();
  return new Promise<T>((resolve, reject) => {
    const tx = db.transaction(STORE, mode);
    const req = fn(tx.objectStore(STORE));
    // Writes resolve on commit, not on request success, so a quota abort still rejects.
    tx.oncomplete = () => resolve(req.result);
    tx.onerror = () => reject(tx.error ?? req.error);
    tx.onabort = () => reject(tx.error ?? new Error("indexedDB transaction aborted"));
  });
}

// A record carries the revision (call-time clock) of the write that produced it, so a delayed
// delete from another tab can tell it is looking at a newer draft than the one it was clearing.
interface StoredDraft {
  rev: number;
  attachments: readonly PromptAttachmentInput[];
}

export async function loadAttachments(sessionId: string): Promise<unknown> {
  const record = await run<StoredDraft | undefined>("readonly", (s) => s.get(sessionId));
  return record?.attachments;
}

export async function saveAttachments(
  sessionId: string,
  attachments: readonly PromptAttachmentInput[],
  rev = Date.now(),
): Promise<void> {
  const record: StoredDraft = { rev, attachments };
  await run("readwrite", (s) => s.put(record, sessionId));
}

/** Deletes the record unless a write newer than `clearedAt` already replaced it. The check and the
 *  delete share one transaction, so a save from another tab cannot slip in between. */
export async function deleteAttachments(sessionId: string, clearedAt = Infinity): Promise<void> {
  const db = await openDb();
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction(STORE, "readwrite");
    const store = tx.objectStore(STORE);
    const read = store.get(sessionId) as IDBRequest<StoredDraft | undefined>;
    read.onsuccess = () => {
      if (read.result && read.result.rev > clearedAt) return;
      store.delete(sessionId);
    };
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error ?? read.error);
    tx.onabort = () => reject(tx.error ?? new Error("indexedDB transaction aborted"));
  });
}

export async function listAttachmentSessionIds(): Promise<string[]> {
  const keys = await run("readonly", (s) => s.getAllKeys());
  return keys.filter((k): k is string => typeof k === "string");
}

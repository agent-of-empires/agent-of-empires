// Staged composer attachments in IndexedDB, one record per session id. localStorage's ~5 MB
// quota cannot hold a photo's base64, IndexedDB's can. Every call rejects when IndexedDB is
// unavailable or the write fails, so callers decide how to degrade.

import type { PromptAttachmentInput } from "./acpTypes";

// A new name isolates this protocol (tuple revisions, `S:/C:` markers) from older tabs that still
// run the first IndexedDB version, so neither side can read or rewrite the other's data.
const DB_NAME = "aoe-composer-attachments-v2";
const LEGACY_DB_NAME = "aoe-composer-attachments";
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

// Every record carries the revision of the write that produced it. Revisions are totally ordered
// across tabs (clock first, tab id as tiebreak), and each mutation is one transaction that checks
// the stored revision, so a stale save, a delayed delete or a failed-save cleanup from any tab
// can never replace or remove a newer draft.
export type Revision = readonly [clock: number, tabId: string];

const TAB_ID = Math.random().toString(36).slice(2);
let lastClock = 0;

/** A revision strictly newer than any this tab issued before. The clock has sub-millisecond
 *  resolution so that revisions from different tabs practically never tie. */
export function nextRevision(): Revision {
  const now = typeof performance === "undefined" ? Date.now() : performance.timeOrigin + performance.now();
  lastClock = Math.max(now, lastClock + 0.001);
  return [lastClock, TAB_ID];
}

export function compareRevisions(a: Revision, b: Revision): number {
  if (a[0] !== b[0]) return a[0] - b[0];
  return a[1] < b[1] ? -1 : a[1] > b[1] ? 1 : 0;
}

interface StoredDraft {
  rev: Revision;
  attachments: readonly PromptAttachmentInput[];
}

export async function loadAttachments(sessionId: string): Promise<unknown> {
  const record = await run<StoredDraft | undefined>("readonly", (s) => s.get(sessionId));
  return record?.attachments;
}

/** The newest state in the store: its revision, and whether it is a draft or a clear (tombstone). */
export async function loadState(sessionId: string): Promise<{ rev: Revision; saved: boolean } | undefined> {
  const record = await run<StoredDraft | undefined>("readonly", (s) => s.get(sessionId));
  return record && { rev: record.rev, saved: record.attachments.length > 0 };
}

/** Drops the database of the first IndexedDB version, which nothing reads any more. */
export async function deleteLegacyDatabase(): Promise<void> {
  if (typeof indexedDB === "undefined") return;
  await new Promise<void>((resolve, reject) => {
    const req = indexedDB.deleteDatabase(LEGACY_DB_NAME);
    req.onsuccess = () => resolve();
    req.onerror = () => reject(req.error);
    req.onblocked = () => resolve();
  });
}

/** Reads the current record inside one readwrite transaction and lets `apply` mutate or skip. */
async function mutate(sessionId: string, apply: (store: IDBObjectStore, current: StoredDraft | undefined) => void) {
  const db = await openDb();
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction(STORE, "readwrite");
    const store = tx.objectStore(STORE);
    const read = store.get(sessionId) as IDBRequest<StoredDraft | undefined>;
    let failure: unknown;
    read.onsuccess = () => {
      try {
        apply(store, read.result);
      } catch (err) {
        failure = err;
        tx.abort();
      }
    };
    // Resolve on commit, so a quota abort still rejects.
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error ?? read.error);
    tx.onabort = () => reject(failure ?? tx.error ?? new Error("indexedDB transaction aborted"));
  });
}

/** Stores the draft unless a newer revision is already there. */
export async function saveAttachments(
  sessionId: string,
  attachments: readonly PromptAttachmentInput[],
  rev: Revision = nextRevision(),
): Promise<void> {
  const record: StoredDraft = { rev, attachments };
  await mutate(sessionId, (store, current) => {
    if (current && compareRevisions(current.rev, rev) > 0) return;
    store.put(record, sessionId);
  });
}

/** Records a clear as a tombstone with its revision, unless a newer revision is already there. The
 *  revision is what lets a late older save from any tab see, inside its own transaction, that the
 *  draft was cleared after it was issued. */
export async function clearAttachments(sessionId: string, rev: Revision): Promise<void> {
  const tombstone: StoredDraft = { rev, attachments: [] };
  await mutate(sessionId, (store, current) => {
    if (current && compareRevisions(current.rev, rev) > 0) return;
    store.put(tombstone, sessionId);
  });
}

/** Removes the record outright, tombstone included. Only for sessions that no longer exist. */
export async function deleteAttachments(sessionId: string): Promise<void> {
  await run("readwrite", (s) => s.delete(sessionId));
}

export async function listAttachmentSessionIds(): Promise<string[]> {
  const keys = await run("readonly", (s) => s.getAllKeys());
  return keys.filter((k): k is string => typeof k === "string");
}

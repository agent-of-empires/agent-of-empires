// Structured view composer drafts in localStorage (`acp:draft:<session_id>`) with pub/sub for UI such as the sidebar draft dot.

import { useMemo, useSyncExternalStore } from "react";

import type { PromptAttachmentInput } from "./acpTypes";
import {
  clearAttachments,
  deleteAttachments,
  deleteAttachmentsUpTo,
  listAttachmentSessionIds,
  loadAttachments,
  nextRevision,
  saveAttachments,
  compareRevisions,
  loadState,
  deleteLegacyDatabase,
  type Revision,
} from "./attachmentStore";
import { safeGetItem, safeRemoveItem, safeSetItem } from "./safeStorage";
import { toastBus } from "./toastBus";

const DRAFT_KEY_PREFIX = "acp:draft:";
// Attachments use their own key: text writes are debounced per keystroke while attachments write on stage/remove.
// The value says whether a draft exists, and carries the revision of the latest save or clear, so
// operations from any tab order against it: a late older save cannot resurrect a cleared draft,
// and a failed save cannot clobber a newer one. The IndexedDB record holds only the bytes and is
// cleanup-only without a saved marker, so an aborted delete cannot bring a sent draft back.
const ATTACHMENT_KEY_PREFIX = "acp:draft-attachments-v2:";
// First-version keys (bare `1` markers or inline JSON): other tabs may still write them, nothing here reads them.
const LEGACY_ATTACHMENT_KEY_PREFIX = "acp:draft-attachments:";

interface AttachmentMarker {
  saved: boolean;
  rev: Revision;
}

/** `S:<clock>:<tab>` saved, `C:<clock>:<tab>` cleared. */
function parseMarker(raw: string | null): AttachmentMarker | null {
  const m = raw?.match(/^([SC]):([0-9.e+]+):([^:]*)$/);
  if (!m) return null;
  return { saved: m[1] === "S", rev: [Number(m[2]), m[3] ?? ""] };
}

function readMarker(sessionId: string): AttachmentMarker | null {
  return parseMarker(safeGetItem(attachmentKey(sessionId)));
}

/** Writes the marker unless a newer one is there. The read and the write are separate storage calls,
 *  so another tab can slip in between; `reconcileMarker` repairs that. False only when the write failed. */
function writeMarker(sessionId: string, saved: boolean, rev: Revision): boolean {
  const current = readMarker(sessionId);
  if (current && compareRevisions(current.rev, rev) > 0) return true;
  const value = `${saved ? "S" : "C"}:${rev[0]}:${rev[1]}`;
  if (safeSetItem(attachmentKey(sessionId), value)) return true;
  // Storage refused the write; a leftover saved marker would resurrect the draft, so drop it.
  if (!saved) safeRemoveItem(attachmentKey(sessionId));
  return false;
}

// Sessions that already toasted "storage full"; a successful write re-arms the toast.
const toastedSessions = new Set<string>();

function notifyDraftPersistFailure(sessionId: string): void {
  if (toastedSessions.has(sessionId)) return;
  toastedSessions.add(sessionId);
  toastBus.handler?.error("Storage full: unsent draft not saved. Free space or copy your draft elsewhere.");
}

const toastedRestoreSessions = new Set<string>();

/** A failed read is not "no draft", so say so: nothing staged now will be saved either. */
export function notifyAttachmentRestoreFailure(sessionId: string): void {
  if (toastedRestoreSessions.has(sessionId)) return;
  toastedRestoreSessions.add(sessionId);
  toastBus.handler?.error("Could not load saved attachments. Files you attach now will not survive a reload.");
}

function clearDraftPersistFailure(sessionId: string): void {
  toastedSessions.delete(sessionId);
}

function draftKey(sessionId: string): string {
  return `${DRAFT_KEY_PREFIX}${sessionId}`;
}

function attachmentKey(sessionId: string): string {
  return `${ATTACHMENT_KEY_PREFIX}${sessionId}`;
}

// Covers both text and attachment keys.
function sessionIdFromKey(key: string): string | null {
  if (key.startsWith(DRAFT_KEY_PREFIX)) return key.slice(DRAFT_KEY_PREFIX.length);
  if (key.startsWith(ATTACHMENT_KEY_PREFIX)) return key.slice(ATTACHMENT_KEY_PREFIX.length);
  return null;
}

type Listener = () => void;

// Optional session id filter per listener; null fires on any change, including a cross-tab clear.
const localListeners = new Map<Listener, ReadonlySet<string> | null>();

function notify(sessionId: string | null) {
  for (const [cb, filter] of localListeners) {
    if (filter === null || sessionId === null || filter.has(sessionId)) cb();
  }
}

export function __resetDraftPersistFailureNotifications(): void {
  toastedSessions.clear();
  toastedRestoreSessions.clear();
}

export function getDraft(sessionId: string): string {
  return safeGetItem(draftKey(sessionId)) ?? "";
}

export function setDraft(sessionId: string, text: string): void {
  let ok = true;
  if (text.length === 0) {
    safeRemoveItem(draftKey(sessionId));
  } else {
    ok = safeSetItem(draftKey(sessionId), text);
  }
  if (!ok) {
    // Surface one toast per session so the user knows unsent text is at risk.
    notifyDraftPersistFailure(sessionId);
  } else {
    clearDraftPersistFailure(sessionId);
  }
  notify(sessionId);
}

export function clearDraft(sessionId: string): void {
  setDraft(sessionId, "");
}

function isPromptAttachmentKind(kind: unknown): kind is PromptAttachmentInput["kind"] {
  return kind === "image" || kind === "audio" || kind === "resource";
}

function isPromptAttachmentInput(v: unknown): v is PromptAttachmentInput {
  if (!v || typeof v !== "object" || Array.isArray(v)) return false;
  const r = v as Record<string, unknown>;
  return (
    isPromptAttachmentKind(r.kind) &&
    typeof r.mimeType === "string" &&
    typeof r.dataB64 === "string" &&
    (r.name === undefined || typeof r.name === "string")
  );
}

/** Records a clear as a tombstone; when even that does not fit the quota, frees the stale bytes
 *  instead, so a draft that was cleared or failed to save cannot linger until its session goes. */
async function recordClear(sessionId: string, rev: Revision): Promise<void> {
  try {
    await clearAttachments(sessionId, rev);
  } catch {
    await deleteAttachmentsUpTo(sessionId, rev);
  }
}

/** Runs under a cross-tab lock where the browser has one, and plainly otherwise. */
async function withSessionLock(sessionId: string, fn: () => Promise<void>): Promise<void> {
  const locks = typeof navigator === "undefined" ? undefined : navigator.locks;
  if (!locks) return fn();
  await locks.request(`aoe-attachment-draft:${sessionId}`, fn);
}

/** Makes the marker at least as new as the stored state. Every operation ends here once its
 *  IndexedDB transaction is done, so a marker clobbered by an interleaved write in another tab
 *  converges to the newest committed draft or clear: the stored revision (tombstones included)
 *  is the authoritative order. */
async function reconcileMarker(sessionId: string): Promise<void> {
  try {
    await withSessionLock(sessionId, async () => {
      const state = await loadState(sessionId);
      if (!state) return;
      const current = readMarker(sessionId);
      if (current && compareRevisions(current.rev, state.rev) >= 0) return;
      if (writeMarker(sessionId, state.saved, state.rev)) notify(sessionId);
    });
  } catch {
    // IndexedDB unavailable; the marker keeps what the operation itself wrote.
  }
}

// Per-session write chain so an older write can never land after a newer one.
const attachmentWrites = new Map<string, Promise<void>>();
// Latest write per session, so a failed older write can tell it was superseded.
const attachmentGenerations = new Map<string, number>();

function chainAttachmentWrite(sessionId: string, write: () => Promise<void>): Promise<void> {
  const next = (attachmentWrites.get(sessionId) ?? Promise.resolve()).then(write);
  const tail = next.catch(() => {});
  attachmentWrites.set(sessionId, tail);
  void tail.then(() => {
    if (attachmentWrites.get(sessionId) === tail) attachmentWrites.delete(sessionId);
  });
  return next;
}

/** Null when the read failed, which is not the same as no draft: callers must not overwrite it. */
export async function getDraftAttachments(sessionId: string): Promise<PromptAttachmentInput[] | null> {
  if (!hasDraftAttachments(sessionId)) return [];
  try {
    // Queued writes land first, and a clear among them leaves nothing to restore.
    await attachmentWrites.get(sessionId);
    if (!hasDraftAttachments(sessionId)) return [];
    const stored = await loadAttachments(sessionId);
    toastedRestoreSessions.delete(sessionId);
    const restored = Array.isArray(stored) ? stored.filter(isPromptAttachmentInput) : [];
    // A saved marker over a tombstone (an interrupted tab never repaired it) would keep the draft dot up.
    if (restored.length === 0) void reconcileMarker(sessionId);
    return restored;
  } catch {
    return null;
  }
}

// The bytes live in IndexedDB, together with the revision of the latest save or clear (a clear is
// a tombstone record); localStorage keeps only the marker, so `hasDraft` stays synchronous
// and a write in another tab still fires a `storage` event. A marker that cannot be written
// makes the record undiscoverable, so it counts as a failed save.
export function setDraftAttachments(sessionId: string, attachments: readonly PromptAttachmentInput[]): Promise<void> {
  const generation = (attachmentGenerations.get(sessionId) ?? 0) + 1;
  attachmentGenerations.set(sessionId, generation);
  const rev = nextRevision();
  if (attachments.length === 0) {
    writeMarker(sessionId, false, rev);
    notify(sessionId);
    clearDraftPersistFailure(sessionId);
    // The delete may queue behind a save, so it only removes what existed when the clear happened.
    return chainAttachmentWrite(sessionId, async () => {
      await recordClear(sessionId, rev);
      await reconcileMarker(sessionId);
    }).catch(() => {});
  }
  const marked = writeMarker(sessionId, true, rev);
  notify(sessionId);
  return chainAttachmentWrite(sessionId, async () => {
    try {
      if (!marked) throw new Error("draft marker not written");
      await saveAttachments(sessionId, attachments, rev);
      clearDraftPersistFailure(sessionId);
      await reconcileMarker(sessionId);
    } catch {
      // A newer write or a clear in this tab owns the state now, and must not be warned about.
      if (attachmentGenerations.get(sessionId) !== generation) return;
      // Exact-or-none: replace this write's record with a clear at its revision, which leaves any
      // newer state (another tab's draft) alone.
      await recordClear(sessionId, rev).catch(() => {});
      writeMarker(sessionId, false, rev);
      notifyDraftPersistFailure(sessionId);
      notify(sessionId);
      await reconcileMarker(sessionId);
    }
  });
}

/** Restores for one session run one at a time, so a replacement mount reads only after the
 *  previous mount's reconciliation has queued its write. */
const restoreLocks = new Map<string, Promise<void>>();

export function exclusiveRestore(sessionId: string, restore: () => Promise<void>): Promise<void> {
  const run = (restoreLocks.get(sessionId) ?? Promise.resolve()).then(restore);
  const tail = run.catch(() => {});
  restoreLocks.set(sessionId, tail);
  void tail.then(() => {
    if (restoreLocks.get(sessionId) === tail) restoreLocks.delete(sessionId);
  });
  return run;
}

export function hasDraftAttachments(sessionId: string): boolean {
  return readMarker(sessionId)?.saved === true;
}

// Remove drafts for sessions not in the active set (deleted in another tab or device).
export function sweepOrphanDrafts(activeSessionIds: ReadonlySet<string>): void {
  if (typeof window === "undefined") return;
  const toRemove: string[] = [];
  const legacy: string[] = [];
  try {
    for (let i = 0; i < window.localStorage.length; i++) {
      const k = window.localStorage.key(i);
      if (!k) continue;
      const sid = sessionIdFromKey(k);
      if (sid === null) {
        if (k.startsWith(LEGACY_ATTACHMENT_KEY_PREFIX)) legacy.push(k);
        continue;
      }
      if (!activeSessionIds.has(sid)) toRemove.push(k);
    }
    for (const k of [...toRemove, ...legacy]) window.localStorage.removeItem(k);
  } catch {
    // localStorage blocked; the sweep is best-effort.
  }
  if (toRemove.length > 0) notify(null);
  void sweepOrphanAttachmentRecords(activeSessionIds);
  void deleteLegacyDatabase().catch(() => {});
}

async function sweepOrphanAttachmentRecords(activeSessionIds: ReadonlySet<string>): Promise<void> {
  try {
    for (const sid of await listAttachmentSessionIds()) {
      if (!activeSessionIds.has(sid)) await chainAttachmentWrite(sid, () => deleteAttachments(sid));
    }
  } catch {
    // IndexedDB unavailable; the sweep is best-effort.
  }
}

export function hasDraft(sessionId: string): boolean {
  const v = safeGetItem(draftKey(sessionId));
  if (v !== null && v.length > 0) return true;
  // Attachment-only drafts are unsent work too.
  return hasDraftAttachments(sessionId);
}

export function subscribeDrafts(cb: Listener, filter: ReadonlySet<string> | null = null): () => void {
  localListeners.set(cb, filter);
  const onStorage = (e: StorageEvent) => {
    // A null key means localStorage.clear() in another tab.
    if (e.key === null) {
      cb();
      return;
    }
    const sid = sessionIdFromKey(e.key);
    if (sid === null) return;
    if (filter === null || filter.has(sid)) cb();
  };
  window.addEventListener("storage", onStorage);
  return () => {
    localListeners.delete(cb);
    window.removeEventListener("storage", onStorage);
  };
}

export function useHasDraftForSessions(sessionIds: readonly string[]): boolean {
  // A stable primitive key so getSnapshot doesn't tear.
  const ids = sessionIds.join("|");
  const subscribe = useMemo(() => {
    const filter = new Set(ids ? ids.split("|").filter(Boolean) : []);
    return (cb: Listener) => subscribeDrafts(cb, filter);
  }, [ids]);
  return useSyncExternalStore(
    subscribe,
    () => {
      for (const id of ids ? ids.split("|") : []) {
        if (id && hasDraft(id)) return true;
      }
      return false;
    },
    () => false,
  );
}

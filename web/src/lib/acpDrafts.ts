// Structured view composer drafts in localStorage (`acp:draft:<session_id>`) with pub/sub for UI such as the sidebar draft dot.

import { useMemo, useSyncExternalStore } from "react";

import type { PromptAttachmentInput } from "./acpTypes";
import { deleteAttachments, listAttachmentSessionIds, loadAttachments, saveAttachments } from "./attachmentStore";
import { safeGetItem, safeRemoveItem, safeSetItem } from "./safeStorage";
import { toastBus } from "./toastBus";

const DRAFT_KEY_PREFIX = "acp:draft:";
// Attachments use their own key: text writes are debounced per keystroke while attachments write on stage/remove.
// The value is only a marker that IndexedDB holds the bytes. It decides whether a draft exists; the
// record is cleanup-only without it, so an aborted delete cannot bring a sent draft back.
const ATTACHMENT_KEY_PREFIX = "acp:draft-attachments:";
const ATTACHMENT_MARKER = "1";

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
  const raw = safeGetItem(attachmentKey(sessionId));
  if (raw !== ATTACHMENT_MARKER) {
    // An older JSON payload in the marker key is dead weight now; drop it.
    if (raw !== null) safeRemoveItem(attachmentKey(sessionId));
    return [];
  }
  try {
    // Queued writes land first, and a clear among them leaves nothing to restore.
    await attachmentWrites.get(sessionId);
    if (!hasDraftAttachments(sessionId)) return [];
    const stored = await loadAttachments(sessionId);
    toastedRestoreSessions.delete(sessionId);
    return Array.isArray(stored) ? stored.filter(isPromptAttachmentInput) : [];
  } catch {
    return null;
  }
}

// The bytes live in IndexedDB; localStorage keeps only a marker, so `hasDraft` stays synchronous
// and a write in another tab still fires a `storage` event. A marker that cannot be written
// makes the record undiscoverable, so it counts as a failed save.
export function setDraftAttachments(sessionId: string, attachments: readonly PromptAttachmentInput[]): Promise<void> {
  const key = attachmentKey(sessionId);
  const generation = (attachmentGenerations.get(sessionId) ?? 0) + 1;
  attachmentGenerations.set(sessionId, generation);
  if (attachments.length === 0) {
    safeRemoveItem(key);
    notify(sessionId);
    clearDraftPersistFailure(sessionId);
    // The delete may queue behind a save, so it only removes what was there when the clear happened.
    const clearedAt = Date.now();
    return chainAttachmentWrite(sessionId, () => deleteAttachments(sessionId, clearedAt)).catch(() => {});
  }
  const marked = safeSetItem(key, ATTACHMENT_MARKER);
  notify(sessionId);
  const rev = Date.now();
  return chainAttachmentWrite(sessionId, async () => {
    try {
      if (!marked) throw new Error("draft marker not written");
      await saveAttachments(sessionId, attachments, rev);
      clearDraftPersistFailure(sessionId);
    } catch {
      // An older failing write must not take the marker of a newer write or a clear behind it,
      // nor warn about a draft that no longer exists.
      if (attachmentGenerations.get(sessionId) !== generation) return;
      // Exact-or-none: never leave a stale older attachment set to be restored and re-sent.
      safeRemoveItem(key);
      await deleteAttachments(sessionId).catch(() => {});
      notifyDraftPersistFailure(sessionId);
      notify(sessionId);
    }
  });
}

export function hasDraftAttachments(sessionId: string): boolean {
  return safeGetItem(attachmentKey(sessionId)) === ATTACHMENT_MARKER;
}

// Remove drafts for sessions not in the active set (deleted in another tab or device).
export function sweepOrphanDrafts(activeSessionIds: ReadonlySet<string>): void {
  if (typeof window === "undefined") return;
  const toRemove: string[] = [];
  try {
    for (let i = 0; i < window.localStorage.length; i++) {
      const k = window.localStorage.key(i);
      if (!k) continue;
      const sid = sessionIdFromKey(k);
      if (sid === null) continue;
      if (!activeSessionIds.has(sid)) toRemove.push(k);
    }
    for (const k of toRemove) window.localStorage.removeItem(k);
  } catch {
    // localStorage blocked; the sweep is best-effort.
  }
  if (toRemove.length > 0) notify(null);
  void sweepOrphanAttachmentRecords(activeSessionIds);
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

// Structured view composer drafts in localStorage (`acp:draft:<session_id>`) with pub/sub for UI such as the sidebar draft dot.

import { useMemo, useSyncExternalStore } from "react";

import type { PromptAttachmentInput } from "./acpTypes";
import { safeGetItem, safeRemoveItem, safeSetItem } from "./safeStorage";
import { toastBus } from "./toastBus";

const DRAFT_KEY_PREFIX = "acp:draft:";
// Attachments use their own key: text writes are debounced per keystroke while attachments write on stage/remove.
const ATTACHMENT_KEY_PREFIX = "acp:draft-attachments:";

// Sessions that already toasted "storage full"; a successful write re-arms the toast.
const toastedSessions = new Set<string>();

function notifyDraftPersistFailure(sessionId: string): void {
  if (toastedSessions.has(sessionId)) return;
  toastedSessions.add(sessionId);
  toastBus.handler?.error("Storage full: unsent draft not saved. Free space or copy your draft elsewhere.");
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

export function getDraftAttachments(sessionId: string): PromptAttachmentInput[] {
  const raw = safeGetItem(attachmentKey(sessionId));
  if (!raw) return [];
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(isPromptAttachmentInput);
  } catch {
    return [];
  }
}

export function setDraftAttachments(sessionId: string, attachments: readonly PromptAttachmentInput[]): void {
  const key = attachmentKey(sessionId);
  let ok = true;
  if (attachments.length === 0) {
    safeRemoveItem(key);
  } else {
    let json = "";
    try {
      json = JSON.stringify(attachments);
    } catch {
      ok = false;
    }
    if (ok) ok = safeSetItem(key, json);
    // Exact-or-none: never leave a stale older attachment set to be restored and re-sent.
    if (!ok) safeRemoveItem(key);
  }
  if (!ok) {
    notifyDraftPersistFailure(sessionId);
  } else {
    clearDraftPersistFailure(sessionId);
  }
  notify(sessionId);
}

export function clearDraftAttachments(sessionId: string): void {
  setDraftAttachments(sessionId, []);
}

// Avoids parsing base64 on the sidebar hot path; a non-empty array is longer than "[]".
export function hasDraftAttachments(sessionId: string): boolean {
  const v = safeGetItem(attachmentKey(sessionId));
  return v !== null && v.length > 2 && v.startsWith("[");
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

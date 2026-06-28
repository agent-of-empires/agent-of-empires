// Structured view composer drafts live in localStorage under one key per session
// (`acp:draft:<session_id>`). This module centralises the storage
// shape and exposes a tiny pub/sub so non-composer UI (e.g. the sidebar
// "unsent draft" dot) can react to writes from any tab.

import { useMemo, useSyncExternalStore } from "react";

import type { PromptAttachmentInput } from "./acpTypes";
import { safeGetItem, safeRemoveItem, safeSetItem } from "./safeStorage";
import { toastBus } from "./toastBus";

const DRAFT_KEY_PREFIX = "acp:draft:";
// Staged composer attachments persist beside the text draft under their own
// key (JSON array of PromptAttachmentInput) so an unsent multimodal prompt
// survives session switches and reloads, matching the text-draft contract.
// Kept parallel to the text key, not folded into one JSON blob: text writes
// on a 250ms keystroke debounce while attachments write on stage/remove, and
// a single blob would make the two write paths clobber each other. Parallel
// keys also let text persist even when a large base64 image blows quota.
const ATTACHMENT_KEY_PREFIX = "acp:draft-attachments:";

// Sessions that have already surfaced a "storage full" toast this page
// load. We dedupe so the composer does not toast on every keystroke once
// storage is full. A successful write for the same session id clears the
// flag, so a later exhaustion event after the user frees space surfaces
// a fresh toast. Per-session granularity means two sessions failing in
// parallel each get their own (single) toast. See #1345.
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

// Recognizes both the text and attachment key prefixes so the orphan sweep
// and the cross-tab storage listener cover attachment drafts for free.
function sessionIdFromKey(key: string): string | null {
  if (key.startsWith(DRAFT_KEY_PREFIX)) return key.slice(DRAFT_KEY_PREFIX.length);
  if (key.startsWith(ATTACHMENT_KEY_PREFIX)) return key.slice(ATTACHMENT_KEY_PREFIX.length);
  return null;
}

type Listener = () => void;

// Each listener may register an optional id filter. When present, the
// listener only fires for changes to a draft whose session id is in the
// set; null means "fire for any draft change" (and for cross-tab
// `localStorage.clear()`, where we don't know which keys went away).
const localListeners = new Map<Listener, ReadonlySet<string> | null>();

function notify(sessionId: string | null) {
  for (const [cb, filter] of localListeners) {
    if (filter === null || sessionId === null || filter.has(sessionId)) cb();
  }
}

// Test-only hook for resetting the per-session toast dedupe between
// cases. Not part of the public API.
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
    // Non-empty draft failed to persist. Surface a single toast per
    // session so the user knows their unsent text is at risk.
    notifyDraftPersistFailure(sessionId);
  } else {
    // Any successful write (including a removal that clears the draft)
    // resets the dedupe, so a later exhaustion re-toasts.
    clearDraftPersistFailure(sessionId);
  }
  notify(sessionId);
}

// Drop the persisted draft for a single session id. Convenience over
// `setDraft(id, "")`; intended for session-delete paths so callers
// don't have to import an empty-string sentinel.
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
    // Exact-or-none: if the current set cannot be persisted (quota, serialize
    // failure), drop the key so a stale older draft is never restored and
    // silently re-sent. The in-memory staged attachments stay live for the
    // current page lifetime; the user just loses them on reload.
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

// Cheap presence check for the sidebar "unsent draft" dot: a non-empty
// stored array serializes to more than "[]" (2 chars) and starts with "[",
// so we avoid parsing (potentially megabytes of base64) on the sidebar
// re-render hot path while still rejecting obviously-corrupt non-array
// values (which would otherwise leave the dot stuck on).
export function hasDraftAttachments(sessionId: string): boolean {
  const v = safeGetItem(attachmentKey(sessionId));
  return v !== null && v.length > 2 && v.startsWith("[");
}

// Remove every `acp:draft:<id>` key whose session id is not in the
// given active set. Run once on app mount to catch drafts left behind
// by session deletions that happened in another tab or on another
// device (the local-tab delete path calls `clearDraft` directly).
// Fires a single wildcard notify after the batch so the sidebar's
// "unsent draft" dot recomputes.
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
    /* localStorage blocked; sweep is best-effort */
  }
  if (toRemove.length > 0) notify(null);
}

export function hasDraft(sessionId: string): boolean {
  const v = safeGetItem(draftKey(sessionId));
  if (v !== null && v.length > 0) return true;
  // An attachment-only draft (no text) is still unsent work, so the sidebar
  // dot must light for it too. useHasDraftForSessions reads through here.
  return hasDraftAttachments(sessionId);
}

// Subscribe to draft changes. `filter` scopes the listener to a specific
// set of session ids; pass null/undefined to receive every draft change.
// Fires for writes in the current tab (manually emitted) and for writes
// in other tabs (storage event). Returns an unsubscribe function.
export function subscribeDrafts(cb: Listener, filter: ReadonlySet<string> | null = null): () => void {
  localListeners.set(cb, filter);
  const onStorage = (e: StorageEvent) => {
    // e.key is null when localStorage.clear() is called from another
    // tab; treat that as "everything changed" and unconditionally fire.
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

// Returns true when ANY of the given session ids has a non-empty draft.
// Re-renders the calling component only when one of THESE ids changes,
// not on every structured view draft write anywhere in the app.
export function useHasDraftForSessions(sessionIds: readonly string[]): boolean {
  // Stable join key so getSnapshot returns the same primitive across
  // renders unless the relevant drafts actually change; otherwise
  // useSyncExternalStore would tear under React 18's strict checks.
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

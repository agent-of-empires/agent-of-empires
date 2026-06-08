import {
  safeGetItem,
  safeRemoveItem,
  safeSetItem,
} from "../../../lib/safeStorage";
import type { DiffComment, DiffCommentsStorageV1 } from "./types";

const KEY_PREFIX = "aoe:diff-comments:v1:";

export function storageKey(sessionId: string): string {
  return `${KEY_PREFIX}${sessionId}`;
}

function sessionIdFromKey(key: string): string | null {
  if (!key.startsWith(KEY_PREFIX)) return null;
  return key.slice(KEY_PREFIX.length);
}

export const EMPTY_STORAGE: DiffCommentsStorageV1 = {
  version: 1,
  comments: [],
  clearAfterSend: true,
  introDraft: "",
  outroDraft: "",
};

/** Load comments for a session. Tolerates corruption, missing keys,
 *  and version mismatches by falling back to an empty state. localStorage
 *  is browser-local; data corruption shouldn't kill the feature. */
export function loadComments(sessionId: string): DiffCommentsStorageV1 {
  const raw = safeGetItem(storageKey(sessionId));
  if (!raw) return { ...EMPTY_STORAGE };
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (
      !parsed ||
      typeof parsed !== "object" ||
      (parsed as { version?: number }).version !== 1 ||
      !Array.isArray((parsed as { comments?: unknown }).comments)
    ) {
      return { ...EMPTY_STORAGE };
    }
    const v = parsed as DiffCommentsStorageV1;
    return {
      version: 1,
      comments: v.comments.filter(isWellFormed),
      clearAfterSend:
        typeof v.clearAfterSend === "boolean" ? v.clearAfterSend : true,
      introDraft: typeof v.introDraft === "string" ? v.introDraft : "",
      outroDraft: typeof v.outroDraft === "string" ? v.outroDraft : "",
    };
  } catch {
    return { ...EMPTY_STORAGE };
  }
}

/** An empty state carries no comments and no draft text. `clearAfterSend`
 *  is ignored: it defaults to true and a lone non-default toggle on an
 *  otherwise-empty session is inert until a comment exists (at which point
 *  the state is no longer empty and persists). Treating it as empty matches
 *  user intent (nothing to send) and the acp-drafts precedent (#1358). */
export function isEmptyState(s: DiffCommentsStorageV1): boolean {
  return s.comments.length === 0 && s.introDraft === "" && s.outroDraft === "";
}

export function saveComments(
  sessionId: string,
  state: DiffCommentsStorageV1,
): void {
  // Remove the key for empty state rather than writing an empty record, so
  // sessions the user never commented on don't accumulate junk keys. Both
  // the write-through and the pagehide flush route through here, so this
  // covers every write path at once. Mirrors setDraft's empty-removal (#1358).
  if (isEmptyState(state)) {
    safeRemoveItem(storageKey(sessionId));
    return;
  }
  safeSetItem(storageKey(sessionId), JSON.stringify(state));
}

// Drop the persisted diff-comments for a single session id. Convenience
// over saveComments(id, EMPTY_STORAGE); intended for session-delete paths
// (mirrors clearDraft for structured view composer drafts). Cross-tab / cross-device
// deletes still fall to sweepOrphanComments on the next mount.
export function clearStoredComments(sessionId: string): void {
  safeRemoveItem(storageKey(sessionId));
}

// Remove every `aoe:diff-comments:v1:<id>` key whose session id is not in
// the given active set. Run once on app mount to catch keys left behind by
// session deletions in another tab or on another device, and to retroactively
// clear empty keys written before the empty-removal fix landed. Mirrors
// sweepOrphanDrafts (#1358).
export function sweepOrphanComments(
  activeSessionIds: ReadonlySet<string>,
): void {
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
}

export function isWellFormed(c: unknown): c is DiffComment {
  if (!c || typeof c !== "object") return false;
  const o = c as Record<string, unknown>;
  return (
    typeof o.id === "string" &&
    typeof o.filePath === "string" &&
    (o.side === "old" || o.side === "new") &&
    typeof o.startLine === "number" &&
    typeof o.endLine === "number" &&
    typeof o.body === "string" &&
    typeof o.capturedSnippet === "string" &&
    typeof o.createdAt === "string"
  );
}

import { safeGetItem, safeRemoveItem, safeSetItem } from "../../../lib/safeStorage";
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

/** Corrupt, missing, or other-version data loads as empty. */
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
      clearAfterSend: typeof v.clearAfterSend === "boolean" ? v.clearAfterSend : true,
      introDraft: typeof v.introDraft === "string" ? v.introDraft : "",
      outroDraft: typeof v.outroDraft === "string" ? v.outroDraft : "",
    };
  } catch {
    return { ...EMPTY_STORAGE };
  }
}

/** `clearAfterSend` alone does not make a state worth persisting. */
export function isEmptyState(s: DiffCommentsStorageV1): boolean {
  return s.comments.length === 0 && s.introDraft === "" && s.outroDraft === "";
}

export function saveComments(sessionId: string, state: DiffCommentsStorageV1): void {
  // Remove instead of writing empty records, so untouched sessions leave no keys.
  if (isEmptyState(state)) {
    safeRemoveItem(storageKey(sessionId));
    return;
  }
  safeSetItem(storageKey(sessionId), JSON.stringify(state));
}

export function clearStoredComments(sessionId: string): void {
  safeRemoveItem(storageKey(sessionId));
}

// Drops keys for sessions deleted elsewhere (another tab or device).
export function sweepOrphanComments(activeSessionIds: ReadonlySet<string>): void {
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

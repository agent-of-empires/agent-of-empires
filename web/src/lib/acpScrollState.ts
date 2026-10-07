// Per-session scroll intent persisted so a PWA reopen restores the reader's position.

import { safeGetItem, safeSetItem } from "./safeStorage";

const KEY_PREFIX = "aoe:acp-scroll:v1:";

export interface AcpScrollState {
  stuck: boolean;
  top: number;
}

export function restoredScrollTop(
  saved: AcpScrollState | null,
  stillPinned: boolean,
  scrollHeight: number,
  clientHeight: number,
): number | null {
  if (!saved || saved.stuck) return stillPinned ? scrollHeight : null;
  return Math.max(0, Math.min(saved.top, scrollHeight - clientHeight));
}

export function saveScrollState(sessionId: string, state: AcpScrollState): void {
  if (!sessionId) return;
  safeSetItem(KEY_PREFIX + sessionId, JSON.stringify(state));
}

export function loadScrollState(sessionId: string): AcpScrollState | null {
  if (!sessionId) return null;
  const raw = safeGetItem(KEY_PREFIX + sessionId);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as Partial<AcpScrollState>;
    if (typeof parsed.stuck !== "boolean" || typeof parsed.top !== "number") return null;
    // Reject NaN, Infinity, and negatives from shared, cross-version storage.
    if (!Number.isFinite(parsed.top) || parsed.top < 0) return null;
    return { stuck: parsed.stuck, top: parsed.top };
  } catch {
    return null;
  }
}

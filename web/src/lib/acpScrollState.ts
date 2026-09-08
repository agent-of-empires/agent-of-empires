// Per-session structured-view scroll intent, persisted so a PWA reopen (a hard
// reload of the installed app) lands where the reader left off: pinned to the
// bottom by default, or at their scrolled-up position if they had scrolled up
// when the app was last hidden/closed. Kept separate from the transcript state
// cache (useAcpSession) because it is component-scroll state, not reducer state.

import { safeGetItem, safeSetItem } from "./safeStorage";

const KEY_PREFIX = "aoe:acp-scroll:v1:";

export interface AcpScrollState {
  /** True when the reader was pinned to the bottom (the default). */
  stuck: boolean;
  /** Last scrollTop, used to approximately restore a scrolled-up position. */
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
  // Best-effort: if the write fails (quota), the reader just gets the default.
  safeSetItem(KEY_PREFIX + sessionId, JSON.stringify(state));
}

export function loadScrollState(sessionId: string): AcpScrollState | null {
  if (!sessionId) return null;
  const raw = safeGetItem(KEY_PREFIX + sessionId);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as Partial<AcpScrollState>;
    if (typeof parsed.stuck !== "boolean" || typeof parsed.top !== "number") return null;
    // `typeof x === "number"` still admits NaN, Infinity, and negatives, any of
    // which would be handed straight to `scrollTop` on restore. Storage is
    // shared-origin and survives across versions, so treat a nonsensical offset
    // as no saved position rather than restoring to it.
    if (!Number.isFinite(parsed.top) || parsed.top < 0) return null;
    return { stuck: parsed.stuck, top: parsed.top };
  } catch {
    return null;
  }
}

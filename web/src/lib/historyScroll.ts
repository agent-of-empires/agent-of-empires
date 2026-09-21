// Pure helpers for the structured view's "load earlier" and scroll-up auto-load.

/** Distance from the top that triggers a preload, and the minimum overflow for auto-load. */
export const HISTORY_PRELOAD_PX = 200;

/** Without a cooldown, restoring scroll near the top re-fires the load every frame. */
export const HISTORY_AUTOLOAD_COOLDOWN_MS = 500;

export interface AutoLoadInput {
  scrollTop: number;
  clientHeight: number;
  scrollHeight: number;
  /** Re-armed once the user scrolls away from the top. */
  armed: boolean;
  canLoadEarlier: boolean;
  now: number;
  lastLoadAt: number;
}

export interface AutoLoadDecision {
  armed: boolean;
  fire: boolean;
}

/** Fires only when the transcript overflows, once per arming and cooldown window. */
export function autoLoadDecision(i: AutoLoadInput): AutoLoadDecision {
  const overflowing = i.scrollHeight > i.clientHeight + HISTORY_PRELOAD_PX;
  if (!overflowing || i.scrollTop > HISTORY_PRELOAD_PX) {
    return { armed: true, fire: false };
  }
  if (i.armed && i.canLoadEarlier && i.now - i.lastLoadAt > HISTORY_AUTOLOAD_COOLDOWN_MS) {
    return { armed: false, fire: true };
  }
  return { armed: i.armed, fire: false };
}

/** Absorbs sub-pixel rounding and reflows. */
export const PINNED_BOTTOM_SLOP_PX = 16;

export function isPinnedToBottom(
  scrollTop: number,
  clientHeight: number,
  scrollHeight: number,
  slop: number = PINNED_BOTTOM_SLOP_PX,
): boolean {
  return scrollTop + clientHeight >= scrollHeight - slop;
}

/** Keeps the read position after older rows grow the top; 0 when pinned to the bottom. */
export function scrollRestoreDelta(prevScrollHeight: number, nextScrollHeight: number, atBottom: boolean): number {
  if (atBottom) return 0;
  const delta = nextScrollHeight - prevScrollHeight;
  return delta > 0 ? delta : 0;
}

export type EarlierAction = "reveal" | "fetch" | "none";

/** Reveal already-loaded rows first, then fetch an older page. */
export function earlierAction(canRevealLoaded: boolean, hasMoreOlder: boolean): EarlierAction {
  if (canRevealLoaded) return "reveal";
  if (hasMoreOlder) return "fetch";
  return "none";
}

export function canOfferEarlier(canRevealLoaded: boolean, hasMoreOlder: boolean): boolean {
  return canRevealLoaded || hasMoreOlder;
}

/** A settled load that didn't grow the transcript; left set it would jump the viewport on the next live append. */
export function anchorIsStale(loading: boolean, anchor: number | null, scrollHeight: number): boolean {
  return !loading && anchor != null && anchor === scrollHeight;
}

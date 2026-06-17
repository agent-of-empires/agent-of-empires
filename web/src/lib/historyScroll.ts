// Pure helpers for the structured-view "load earlier" affordance and its
// scroll-up auto-load. Kept DOM-free so the decision logic is unit-tested
// directly; StructuredView / AcpRuntime wire them to refs and the
// viewport. See #2236.

/** Distance from the top (px) at which scrolling up should pull older
 *  history, and the floor of overflow before auto-load is meaningful. */
export const HISTORY_PRELOAD_PX = 200;

/** Minimum gap between auto-loads. The position restore nudges scrollTop
 *  back near the top after each load, and clicking the top-anchored "Load
 *  earlier" button scrolls it into view there too; without a cooldown the
 *  trigger re-fires every frame and the button never settles. */
export const HISTORY_AUTOLOAD_COOLDOWN_MS = 500;

export interface AutoLoadInput {
  scrollTop: number;
  clientHeight: number;
  scrollHeight: number;
  /** Whether a fresh trigger is armed (re-armed once the user scrolls
   *  away from the top). */
  armed: boolean;
  /** Whether there is any older history left to load. */
  canLoadEarlier: boolean;
  now: number;
  /** Timestamp of the last load (button or auto). */
  lastLoadAt: number;
}

export interface AutoLoadDecision {
  /** Armed state to carry forward. */
  armed: boolean;
  /** True when a load should fire now. */
  fire: boolean;
}

/** Decide whether reaching the top should trigger a load. Only meaningful
 *  once the transcript overflows (otherwise there is nothing to scroll up
 *  through and a mount at scrollTop 0 would auto-load spuriously); fires at
 *  most once per arming and once per cooldown window. */
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

/** Scroll delta to add after older rows grow the transcript at the top so
 *  the read position is frozen. 0 when pinned to the bottom (live appends
 *  keep their stick-to-bottom) or when nothing grew. */
export function scrollRestoreDelta(prevScrollHeight: number, nextScrollHeight: number, atBottom: boolean): number {
  if (atBottom) return 0;
  const delta = nextScrollHeight - prevScrollHeight;
  return delta > 0 ? delta : 0;
}

export type EarlierAction = "reveal" | "fetch" | "none";

/** Two-stage "load earlier": reveal rows already in the reducer first,
 *  then fetch the next-older page from the server once those run out. */
export function earlierAction(canRevealLoaded: boolean, hasMoreOlder: boolean): EarlierAction {
  if (canRevealLoaded) return "reveal";
  if (hasMoreOlder) return "fetch";
  return "none";
}

/** Whether the "Load earlier" control should be offered at all. */
export function canOfferEarlier(canRevealLoaded: boolean, hasMoreOlder: boolean): boolean {
  return canRevealLoaded || hasMoreOlder;
}

/** A pending scroll anchor is stale when a load has settled (nothing in
 *  flight) but the transcript never grew (anchor still equals the current
 *  scrollHeight): an empty/failed page or a no-op reveal. Left set, it
 *  would latch onto the next unrelated growth (a live append while
 *  scrolled up) and jump the viewport, so the caller drops it. See #2236. */
export function anchorIsStale(loading: boolean, anchor: number | null, scrollHeight: number): boolean {
  return !loading && anchor != null && anchor === scrollHeight;
}

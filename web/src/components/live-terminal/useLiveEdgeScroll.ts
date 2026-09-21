import { useCallback, useEffect, useRef, type RefObject } from "react";

/** Live capture window in screenfuls, so a peek up lands on real text; within the server's fast-cadence bound. */
export const LIVE_WINDOW_SCREENS = 2;

/** Live-edge following for the scroller: when to pin to the tail, when the reader has detached, and the
 *  reading/live transitions driven by scrolling. */
export function useLiveEdgeScroll({
  scrollerRef,
  lineH,
  rowsRef,
  reading,
  enterReading,
  returnToLive,
  forwardModeRef,
  scheduleViewSync,
}: {
  scrollerRef: RefObject<HTMLDivElement | null>;
  lineH: number;
  rowsRef: RefObject<number>;
  reading: boolean;
  enterReading: (rows: number) => void;
  returnToLive: (rows: number) => void;
  forwardModeRef: RefObject<boolean>;
  scheduleViewSync: () => void;
}) {
  // No programmatic scroll while a finger is down: it cancels the native gesture on iOS.
  const touchActiveRef = useRef(false);
  // Largest container height at the current width; rows derive from it so the keyboard never resizes tmux.
  const latchRef = useRef<{ width: number; maxHeight: number }>({ width: 0, maxHeight: 0 });
  // Pixel top of the cursor row, sticky across mid-redraw frames that hide the cursor.
  const cursorAnchorRef = useRef<number | null>(null);
  useEffect(() => {
    cursorAnchorRef.current = null;
  }, [lineH]);
  // The live-edge scroll target is the bottom, except while the keyboard has shrunk the container: then the
  // cursor is anchored near the viewport bottom so the agent's prompt stays visible.
  const liveScrollTarget = useCallback(
    (el: HTMLDivElement) => {
      const bottom = Math.max(0, el.scrollHeight - el.clientHeight);
      const shrunken = latchRef.current.maxHeight - el.clientHeight > lineH * 1.5;
      const anchor = cursorAnchorRef.current;
      if (!shrunken || anchor == null) return bottom;
      // One spare line keeps the input box border visible.
      return Math.min(bottom, Math.max(0, anchor + 2 * lineH - el.clientHeight));
    },
    [lineH],
  );
  const geomRef = useRef({ target: -1, clientHeight: 0, scrollTop: 0 });
  // Sticky "reading, do not follow the tail" latch. It re-attaches only at the literal bottom (onScroll), on a
  // lift at the bottom (onTouchEnd), or via jump-to-latest; recomputing it per frame yanked paused readers back.
  const liveDetachedRef = useRef(false);
  // Swallows the scroll event from a programmatic return to live before `reading` catches up.
  const forceLiveRef = useRef(false);
  // A keyboard height change seen while pinning was suppressed, applied at the next pin that can run.
  const pendingHeightPinRef = useRef(false);
  const pinIfWasAtBottom = useCallback(() => {
    const el = scrollerRef.current;
    if (!el) return;
    const prev = geomRef.current;
    const target = liveScrollTarget(el);
    const heightChanged = prev.target >= 0 && Math.abs(el.clientHeight - prev.clientHeight) > 1;
    const movingUp = prev.target >= 0 && el.scrollTop < prev.scrollTop - 0.5;
    // A real scroll-up moved up and sits clearly above the target; appends, content-shrink clamps, and keyboard
    // height changes also lower scrollTop relative to the target but are not the user's doing.
    if (!heightChanged && movingUp && el.scrollTop < target - 2) liveDetachedRef.current = true;
    if (heightChanged) pendingHeightPinRef.current = true;
    if (liveDetachedRef.current) {
      pendingHeightPinRef.current = false;
    } else if (
      !touchActiveRef.current &&
      // Following the tail must not fire in the first pixels of an upward flick, or it cancels iOS momentum.
      (prev.target < 0 || pendingHeightPinRef.current || (!movingUp && target > el.scrollTop))
    ) {
      el.scrollTop = target;
      pendingHeightPinRef.current = false;
    }
    geomRef.current = { target, clientHeight: el.clientHeight, scrollTop: el.scrollTop };
  }, [scrollerRef, liveScrollTarget]);

  const atBottom = useCallback(() => {
    const el = scrollerRef.current;
    return !el || el.scrollTop >= liveScrollTarget(el) - lineH * 1.5;
  }, [scrollerRef, lineH, liveScrollTarget]);

  const onScrollLastTopRef = useRef(0);
  const onScroll = useCallback(() => {
    scheduleViewSync();
    if (forwardModeRef.current) return;
    const el = scrollerRef.current;
    if (!el) return;
    const movingUp = el.scrollTop < onScrollLastTopRef.current - 0.5;
    onScrollLastTopRef.current = el.scrollTop;
    if (forceLiveRef.current) {
      if (reading) return returnToLive(rowsRef.current * LIVE_WINDOW_SCREENS);
      forceLiveRef.current = false;
    }
    if (!atBottom()) enterReading(rowsRef.current);
    // Mid-gesture passes over the bottom settle on touchend instead.
    else if (!touchActiveRef.current) returnToLive(rowsRef.current * LIVE_WINDOW_SCREENS);
    // Scrolling down to the literal bottom is where auto-follow resumes for mouse scrolling.
    if (el.scrollHeight - el.clientHeight - el.scrollTop < 2 && !movingUp) liveDetachedRef.current = false;
  }, [scrollerRef, rowsRef, atBottom, enterReading, forwardModeRef, reading, returnToLive, scheduleViewSync]);

  const jumpToLatest = useCallback(() => {
    const el = scrollerRef.current;
    if (el) el.scrollTop = liveScrollTarget(el);
    liveDetachedRef.current = false;
    // Dropping the selection releases a held frame.
    document.getSelection()?.removeAllRanges();
    returnToLive(rowsRef.current * LIVE_WINDOW_SCREENS);
  }, [scrollerRef, rowsRef, returnToLive, liveScrollTarget]);

  return {
    touchActiveRef,
    latchRef,
    cursorAnchorRef,
    liveDetachedRef,
    forceLiveRef,
    liveScrollTarget,
    pinIfWasAtBottom,
    atBottom,
    onScroll,
    jumpToLatest,
  };
}

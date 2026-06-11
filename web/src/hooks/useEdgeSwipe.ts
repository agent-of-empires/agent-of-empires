import { useEffect } from "react";

interface EdgeSwipeOptions {
  edge: "left" | "right";
  enabled: boolean;
  onSwipe: () => void;
  /** Before invoking onSwipe, blur the currently focused element. */
  blurOnSwipe?: boolean;
  /** Allow the gesture to start anywhere on screen, not just within the edge
   *  zone. Used so a full-width swipe-right opens the sidebar. A larger
   *  horizontal threshold keeps it from firing on incidental drags. */
  anywhere?: boolean;
}

const EDGE_PX = 24;
const THRESHOLD_PX = 60;
// A from-anywhere swipe needs to travel further (and stay clearly horizontal)
// so it doesn't trigger on a stray drag mid-screen.
const ANYWHERE_THRESHOLD_PX = 90;
const VERTICAL_CANCEL_PX = 16;
const MOBILE_BREAKPOINT = 768;

/**
 * Detect a one-finger swipe that moves horizontally past a threshold. By
 * default the gesture must start in the left/right edge zone (used to open the
 * diff/right panel); with `anywhere`, it may start anywhere on screen (used so
 * a full-width swipe-right opens the sidebar). Mobile-only (below 768px).
 */
export function useEdgeSwipe({ edge, enabled, onSwipe, blurOnSwipe = false, anywhere = false }: EdgeSwipeOptions) {
  useEffect(() => {
    if (!enabled) return;

    let startX = 0;
    let startY = 0;
    let tracking = false;
    const threshold = anywhere ? ANYWHERE_THRESHOLD_PX : THRESHOLD_PX;

    const onTouchStart = (e: TouchEvent) => {
      if (window.innerWidth >= MOBILE_BREAKPOINT || e.touches.length !== 1) return;
      const t = e.touches[0];
      if (!t) return;
      const inEdge = edge === "left" ? t.clientX <= EDGE_PX : t.clientX >= window.innerWidth - EDGE_PX;
      if (!anywhere && !inEdge) return;
      tracking = true;
      startX = t.clientX;
      startY = t.clientY;
    };

    const onTouchMove = (e: TouchEvent) => {
      if (!tracking) return;
      const t = e.touches[0];
      if (!t) return;
      const dx = edge === "left" ? t.clientX - startX : startX - t.clientX;
      const dy = t.clientY - startY;
      if (dx > threshold && Math.abs(dx) > Math.abs(dy)) {
        tracking = false;
        if (blurOnSwipe && document.activeElement instanceof HTMLElement) {
          document.activeElement.blur();
        }
        onSwipe();
      } else if (Math.abs(dy) > Math.abs(dx) && Math.abs(dy) > VERTICAL_CANCEL_PX) {
        tracking = false;
      }
    };

    const onTouchEnd = () => {
      tracking = false;
    };

    window.addEventListener("touchstart", onTouchStart, { passive: true });
    window.addEventListener("touchmove", onTouchMove, { passive: true });
    window.addEventListener("touchend", onTouchEnd, { passive: true });
    window.addEventListener("touchcancel", onTouchEnd, { passive: true });
    return () => {
      window.removeEventListener("touchstart", onTouchStart);
      window.removeEventListener("touchmove", onTouchMove);
      window.removeEventListener("touchend", onTouchEnd);
      window.removeEventListener("touchcancel", onTouchEnd);
    };
  }, [edge, enabled, onSwipe, blurOnSwipe, anywhere]);
}

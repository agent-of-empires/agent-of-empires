import { useEffect } from "react";

interface EdgeSwipeOptions {
  edge: "left" | "right";
  enabled: boolean;
  onSwipe: () => void;
  /** Before invoking onSwipe, blur the currently focused element. */
  blurOnSwipe?: boolean;
}

const EDGE_PX = 24;
const THRESHOLD_PX = 60;
const VERTICAL_CANCEL_PX = 16;
const MOBILE_BREAKPOINT = 768;

/**
 * Detect a one-finger swipe that starts at the left or right edge of the
 * viewport and moves horizontally past a threshold. Used to open the sidebar
 * (left edge) and the diff/right panel (right edge) on mobile.
 */
export function useEdgeSwipe({ edge, enabled, onSwipe, blurOnSwipe = false }: EdgeSwipeOptions) {
  useEffect(() => {
    if (!enabled) return;

    let startX = 0;
    let startY = 0;
    let tracking = false;

    const onTouchStart = (e: TouchEvent) => {
      if (window.innerWidth >= MOBILE_BREAKPOINT || e.touches.length !== 1) return;
      const t = e.touches[0];
      if (!t) return;
      const inEdge = edge === "left" ? t.clientX <= EDGE_PX : t.clientX >= window.innerWidth - EDGE_PX;
      if (!inEdge) return;
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
      if (dx > THRESHOLD_PX && Math.abs(dx) > Math.abs(dy)) {
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
  }, [edge, enabled, onSwipe, blurOnSwipe]);
}

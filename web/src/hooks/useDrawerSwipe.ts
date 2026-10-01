import { useEffect } from "react";
import { useLatestRef } from "./useLatestRef";

export type SwipeDirection = "left" | "right";
export type DrawerSwipeAction = "open-sidebar" | "close-sidebar" | "open-panels" | "close-panels";

export interface DrawerSwipeState {
  sidebarOpen: boolean;
  sidebarSide: "left" | "right";
  panelsOpen: boolean;
  panelsAvailable: boolean;
}

/** A drawer opens with a swipe away from its edge and closes with a swipe back
 *  toward it. The panels drawer is always on the right; one drawer at a time. */
export function drawerSwipeAction(dir: SwipeDirection, s: DrawerSwipeState): DrawerSwipeAction | null {
  if (s.panelsOpen) return dir === "right" ? "close-panels" : null;
  if (s.sidebarOpen) return dir === s.sidebarSide ? "close-sidebar" : null;
  // A right-side sidebar has no open swipe: swipe-left belongs to the panels.
  if (dir === "right") return s.sidebarSide === "left" ? "open-sidebar" : null;
  return s.panelsAvailable ? "open-panels" : null;
}

// iOS reserves these strips for system back and forward navigation.
const SYSTEM_EDGE_GUARD_PX = 32;
const THRESHOLD_PX = 90;
const VERTICAL_CANCEL_PX = 16;
const MOBILE_BREAKPOINT = 768;

/** Mobile horizontal swipes that open and close the side drawers. */
export function useDrawerSwipe(state: DrawerSwipeState, onAction: (action: DrawerSwipeAction) => void) {
  const latestState = useLatestRef(state);
  const latestOnAction = useLatestRef(onAction);

  useEffect(() => {
    let startX = 0;
    let startY = 0;
    let tracking = false;

    const onTouchStart = (e: TouchEvent) => {
      tracking = false;
      if (window.innerWidth >= MOBILE_BREAKPOINT || e.touches.length !== 1) return;
      const t = e.touches[0];
      if (!t) return;
      if (t.clientX <= SYSTEM_EDGE_GUARD_PX || t.clientX >= window.innerWidth - SYSTEM_EDGE_GUARD_PX) return;
      tracking = true;
      startX = t.clientX;
      startY = t.clientY;
    };

    const onTouchMove = (e: TouchEvent) => {
      if (!tracking) return;
      const t = e.touches[0];
      if (!t) return;
      const dx = t.clientX - startX;
      const dy = t.clientY - startY;
      if (Math.abs(dx) > THRESHOLD_PX && Math.abs(dx) > Math.abs(dy)) {
        tracking = false;
        const action = drawerSwipeAction(dx > 0 ? "right" : "left", latestState.current);
        if (!action) return;
        // Dismiss the on-screen keyboard so it does not cover the drawer.
        if (document.activeElement instanceof HTMLElement) document.activeElement.blur();
        latestOnAction.current(action);
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
  }, [latestState, latestOnAction]);
}

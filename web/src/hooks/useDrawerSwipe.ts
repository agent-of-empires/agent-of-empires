import { useEffect } from "react";
import { useLatestRef } from "./useLatestRef";

export type SwipeDirection = "left" | "right";
export type DrawerSwipeAction = "open-sidebar" | "close-sidebar" | "open-panels" | "close-panels";

export interface DrawerSwipeState {
  sidebarOpen: boolean;
  panelsOpen: boolean;
  panelsAvailable: boolean;
}

/** A drawer opens with a swipe away from its edge and closes with a swipe back toward it: the session drawer
 *  is on the left, the panels on the right, and one is open at a time. */
export function drawerSwipeAction(dir: SwipeDirection, s: DrawerSwipeState): DrawerSwipeAction | null {
  if (s.panelsOpen) return dir === "right" ? "close-panels" : null;
  if (s.sidebarOpen) return dir === "left" ? "close-sidebar" : null;
  if (dir === "right") return "open-sidebar";
  return s.panelsAvailable ? "open-panels" : null;
}

// iOS reserves these strips for system back and forward navigation.
const SYSTEM_EDGE_GUARD_PX = 32;
const THRESHOLD_PX = 90;
const VERTICAL_CANCEL_PX = 16;
const MOBILE_BREAKPOINT = 768;
// A touch that stays within the slop this long is a hold (drag-and-drop,
// long-press), not a swipe. Matches the dnd-kit touch activation constraint.
const HOLD_SLOP_PX = 8;
const HOLD_MS = 150;
// A tracked drag settles open past this share of the drawer, or on a flick this fast (px/ms) its way.
const SETTLE_SHARE = 0.35;
const FLICK_PX_PER_MS = 0.5;

/** Which swipe directions a horizontal scroller under the touch would consume. */
function scrollRoom(path: EventTarget[]): Record<SwipeDirection, boolean> {
  const room = { left: false, right: false };
  for (const node of path) {
    if (!(node instanceof HTMLElement) || node.scrollWidth <= node.clientWidth) continue;
    const { overflowX } = getComputedStyle(node);
    if (overflowX !== "auto" && overflowX !== "scroll") continue;
    // A left swipe scrolls toward the end, a right swipe back toward the start.
    if (node.scrollLeft + node.clientWidth < node.scrollWidth - 1) room.left = true;
    if (node.scrollLeft > 0) room.right = true;
  }
  return room;
}

/**
 * Mobile horizontal swipes that open and close the side drawers. With `onTrack`, the sidebar drawer follows the
 * finger: `onTrack(dx, open)` on every move and `onTrack(null, open)` on release, and the action is decided then.
 */
export function useDrawerSwipe(
  state: DrawerSwipeState,
  onAction: (action: DrawerSwipeAction) => void,
  onTrack?: (dx: number | null, open: boolean) => void,
) {
  const latestState = useLatestRef(state);
  const latestOnAction = useLatestRef(onAction);
  const latestOnTrack = useLatestRef(onTrack);

  useEffect(() => {
    let startX = 0;
    let startY = 0;
    let startTime = 0;
    let moved = false;
    let tracking = false;
    // A sidebar drag that follows the finger, once its direction is known.
    let dragging: { open: boolean; dx: number; at: number; velocity: number } | null = null;
    let room: Record<SwipeDirection, boolean> = { left: false, right: false };

    const onTouchStart = (e: TouchEvent) => {
      tracking = false;
      if (
        window.innerWidth >= MOBILE_BREAKPOINT ||
        e.touches.length !== 1 ||
        document.getSelection()?.isCollapsed === false
      )
        return;
      const t = e.touches[0];
      if (!t) return;
      if (t.clientX <= SYSTEM_EDGE_GUARD_PX || t.clientX >= window.innerWidth - SYSTEM_EDGE_GUARD_PX) return;
      tracking = true;
      dragging = null;
      room = scrollRoom(e.composedPath());
      startX = t.clientX;
      startY = t.clientY;
      startTime = performance.now();
      moved = false;
    };

    const onTouchMove = (e: TouchEvent) => {
      if (!tracking) return;
      if (document.getSelection()?.isCollapsed === false) {
        tracking = false;
        return;
      }
      const t = e.touches[0];
      if (!t) return;
      const dx = t.clientX - startX;
      const dy = t.clientY - startY;
      if (!moved && Math.max(Math.abs(dx), Math.abs(dy)) > HOLD_SLOP_PX) {
        moved = true;
        if (performance.now() - startTime >= HOLD_MS) {
          tracking = false;
          return;
        }
      }
      if (dragging) {
        const now = performance.now();
        dragging.velocity = (dx - dragging.dx) / Math.max(1, now - dragging.at);
        dragging.dx = dx;
        dragging.at = now;
        latestOnTrack.current?.(dx, dragging.open);
        return;
      }
      const s = latestState.current;
      const track = latestOnTrack.current;
      if (track && moved && Math.abs(dx) > HOLD_SLOP_PX && Math.abs(dx) > Math.abs(dy) && !s.panelsOpen) {
        const dir = dx > 0 ? "right" : "left";
        const action = drawerSwipeAction(dir, s);
        if (!room[dir] && (action === "open-sidebar" || action === "close-sidebar")) {
          if (document.activeElement instanceof HTMLElement) document.activeElement.blur();
          dragging = { open: s.sidebarOpen, dx, at: performance.now(), velocity: 0 };
          track(dx, dragging.open);
          return;
        }
      }
      if (Math.abs(dx) > THRESHOLD_PX && Math.abs(dx) > Math.abs(dy)) {
        tracking = false;
        const dir = dx > 0 ? "right" : "left";
        if (room[dir]) return;
        const action = drawerSwipeAction(dir, latestState.current);
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
      if (!dragging) return;
      const { open, dx, velocity } = dragging;
      dragging = null;
      latestOnTrack.current?.(null, open);
      // The drawer's width, as --aoe-drawer-w sets it.
      const width = Math.min(window.innerWidth * 0.85, 400);
      // Toward the open side is positive for a left drawer.
      const settledOpen =
        velocity > FLICK_PX_PER_MS
          ? true
          : velocity < -FLICK_PX_PER_MS
            ? false
            : (open ? width : 0) + dx > width * SETTLE_SHARE;
      if (settledOpen !== open) latestOnAction.current(settledOpen ? "open-sidebar" : "close-sidebar");
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
  }, [latestState, latestOnAction, latestOnTrack]);
}

import { useRef, useState, type MouseEvent as ReactMouseEvent, type PointerEvent as ReactPointerEvent } from "react";

/** Movement before the gesture commits to a direction, so a tap or a scroll never drags. */
const SLOP = 8;

interface Gesture {
  id: number;
  x: number;
  y: number;
  width: number;
  /** null until the gesture commits: true for a horizontal drag toward the edge, false for anything else. */
  dragging: boolean | null;
}

/**
 * Lets the open phone sidebar drawer follow a finger dragging it toward its edge, then hands back to its
 * transition on release. Whether the swipe closes it is `useDrawerSwipe`'s call (the window-level swipe
 * handler); this only makes the drawer track the finger meanwhile. Mouse input and wide screens are left alone.
 */
export function useDrawerDrag({ open, side }: { open: boolean; side: "left" | "right" }) {
  const [drag, setDrag] = useState<{ offset: number; width: number } | null>(null);
  const gesture = useRef<Gesture | null>(null);
  const dragged = useRef(false);
  const toward = side === "left" ? -1 : 1;

  const onPointerDown = (e: ReactPointerEvent<HTMLElement>) => {
    if (!open || e.pointerType === "mouse" || window.matchMedia?.("(min-width: 768px)")?.matches) return;
    dragged.current = false;
    gesture.current = {
      id: e.pointerId,
      x: e.clientX,
      y: e.clientY,
      width: e.currentTarget.getBoundingClientRect().width,
      dragging: null,
    };
  };

  const onPointerMove = (e: ReactPointerEvent<HTMLElement>) => {
    const g = gesture.current;
    if (!g || e.pointerId !== g.id) return;
    const dx = e.clientX - g.x;
    const dy = e.clientY - g.y;
    if (g.dragging === null) {
      if (Math.abs(dx) < SLOP && Math.abs(dy) < SLOP) return;
      g.dragging = Math.abs(dx) > Math.abs(dy) * 1.2 && Math.sign(dx) === toward;
      if (!g.dragging) {
        gesture.current = null;
        return;
      }
      dragged.current = true;
    }
    setDrag({ offset: toward < 0 ? Math.min(0, dx) : Math.max(0, dx), width: g.width });
  };

  const end = (e: ReactPointerEvent<HTMLElement>) => {
    if (gesture.current && e.pointerId !== gesture.current.id) return;
    gesture.current = null;
    setDrag(null);
  };

  return {
    offset: drag?.offset ?? null,
    /** 1 when fully open, falling toward 0 as the drag nears the edge; for the backdrop. */
    progress: drag ? 1 - Math.min(1, Math.abs(drag.offset) / Math.max(1, drag.width)) : 1,
    handlers: {
      onPointerDown,
      onPointerMove,
      onPointerUp: end,
      onPointerCancel: end,
      // A drag that ends over a row must not also open it.
      onClickCapture: (e: ReactMouseEvent) => {
        if (!dragged.current) return;
        dragged.current = false;
        e.preventDefault();
        e.stopPropagation();
      },
    },
  };
}

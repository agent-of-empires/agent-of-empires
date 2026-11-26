import { useRef, useState, type MouseEvent as ReactMouseEvent, type PointerEvent as ReactPointerEvent } from "react";

/** How far past its width, or how fast (px/ms), a release has to travel to close the drawer. */
const CLOSE_FRACTION = 0.3;
const CLOSE_VELOCITY = 0.5;
/** Movement before the gesture commits to a direction, so a tap or a scroll never drags. */
const SLOP = 8;

interface Gesture {
  id: number;
  x: number;
  y: number;
  t: number;
  width: number;
  /** null until the gesture commits: true for a horizontal drag toward the edge, false for anything else. */
  closing: boolean | null;
}

/**
 * Swipe-to-close for the phone sidebar drawer: a horizontal touch drag toward its edge moves it with the
 * finger, and the release closes it past a third of its width or on a flick, else it springs back. Mouse
 * input and wide screens are left alone. `offset` is the drag in px (0 or toward the edge) while dragging.
 */
export function useDrawerSwipe({
  open,
  side,
  onClose,
}: {
  open: boolean;
  side: "left" | "right";
  onClose: () => void;
}) {
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
      t: e.timeStamp,
      width: e.currentTarget.getBoundingClientRect().width,
      closing: null,
    };
  };

  const onPointerMove = (e: ReactPointerEvent<HTMLElement>) => {
    const g = gesture.current;
    if (!g || e.pointerId !== g.id) return;
    const dx = e.clientX - g.x;
    const dy = e.clientY - g.y;
    if (g.closing === null) {
      if (Math.abs(dx) < SLOP && Math.abs(dy) < SLOP) return;
      g.closing = Math.abs(dx) > Math.abs(dy) * 1.2 && Math.sign(dx) === toward;
      if (!g.closing) {
        gesture.current = null;
        return;
      }
      dragged.current = true;
      e.currentTarget.setPointerCapture?.(e.pointerId);
    }
    setDrag({ offset: toward < 0 ? Math.min(0, dx) : Math.max(0, dx), width: g.width });
  };

  const end = (e: ReactPointerEvent<HTMLElement>) => {
    const g = gesture.current;
    gesture.current = null;
    if (!g?.closing || e.pointerId !== g.id) return;
    const dx = (e.clientX - g.x) * toward;
    const velocity = dx / Math.max(1, e.timeStamp - g.t);
    setDrag(null);
    if (dx > g.width * CLOSE_FRACTION || velocity > CLOSE_VELOCITY) onClose();
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

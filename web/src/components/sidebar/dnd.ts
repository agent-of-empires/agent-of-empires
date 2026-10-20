import { useEffect, type MutableRefObject } from "react";
import { closestCenter, type CollisionDetection } from "@dnd-kit/core";

// Long enough for the release click that follows pointer-up, short enough that a browser which
// sends none can't swallow a later one.
const RELEASE_CLICK_MS = 1000;

/**
 * Swallows clicks while a sidebar drag runs and the one click its own release fires, which lands on
 * whatever ends up under the pointer. A new press disarms it, so the next real click goes through.
 */
export interface DragClickGuard {
  dragging: boolean;
  /** Until when the release click may still arrive; 0 once a press or that click has used it. */
  armedUntil: number;
}

export function newDragClickGuard(): DragClickGuard {
  return { dragging: false, armedUntil: 0 };
}

export function dragStarted(guard: DragClickGuard): void {
  guard.dragging = true;
}

export function dragEnded(guard: DragClickGuard, now = Date.now()): void {
  guard.dragging = false;
  guard.armedUntil = now + RELEASE_CLICK_MS;
}

export function pressed(guard: DragClickGuard): void {
  guard.armedUntil = 0;
}

/** Whether to swallow a click now; the release click uses up the arming. */
export function swallowsClick(guard: DragClickGuard, now = Date.now()): boolean {
  if (guard.dragging) return true;
  if (now >= guard.armedUntil) return false;
  guard.armedUntil = 0;
  return true;
}

export function useSuppressClickAfterDrag(ref: MutableRefObject<DragClickGuard>) {
  useEffect(() => {
    const onClick = (e: MouseEvent) => {
      // Keyboard and programmatic clicks (`detail` 0) start with no press, so they would find the guard armed.
      if (e.detail === 0) return;
      if (swallowsClick(ref.current)) {
        e.preventDefault();
        e.stopPropagation();
        e.stopImmediatePropagation();
      }
    };
    const onPress = () => pressed(ref.current);
    // Capture on document: Chromium's post-drag click can bypass React's delegated handlers.
    document.addEventListener("click", onClick, true);
    document.addEventListener("pointerdown", onPress, true);
    return () => {
      document.removeEventListener("click", onClick, true);
      document.removeEventListener("pointerdown", onPress, true);
    };
  }, [ref]);
}

/** Headers and rows share one DndContext, so only droppables of the dragged item's type may collide. */
export const typedClosestCenter: CollisionDetection = (args) => {
  const activeType = args.active.data.current?.type;
  return closestCenter({
    ...args,
    droppableContainers: args.droppableContainers.filter((container) => container.data.current?.type === activeType),
  });
};

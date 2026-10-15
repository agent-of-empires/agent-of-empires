import { createContext, useContext, useEffect, type MutableRefObject } from "react";
import { closestCenter, type CollisionDetection } from "@dnd-kit/core";

/** Timestamp until which clicks are swallowed after a row drag; rows extend it while dragging. */
export const DragSuppressContext = createContext<MutableRefObject<number> | null>(null);

export function useDragSuppressRef(): MutableRefObject<number> {
  const ref = useContext(DragSuppressContext);
  if (!ref) throw new Error("DragSuppressContext used outside provider");
  return ref;
}

export function useSuppressClickAfterDrag(ref: MutableRefObject<number>) {
  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (Date.now() < ref.current) {
        e.preventDefault();
        e.stopPropagation();
        e.stopImmediatePropagation();
      }
    };
    // Capture on document: Chromium's post-drag click can bypass React's delegated handlers.
    document.addEventListener("click", handler, true);
    return () => document.removeEventListener("click", handler, true);
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

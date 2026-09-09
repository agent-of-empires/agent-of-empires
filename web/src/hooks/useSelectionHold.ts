import { useCallback, useLayoutEffect, useRef, useState, useSyncExternalStore } from "react";
import type { RefObject } from "react";

/** Holds the rendered value still while the user holds a text selection
 *  touching `containerRef`, returning the value that was on screen when the
 *  selection began and whether the hold is in effect.
 *
 *  React updates an element whose text changed by rewriting its existing text
 *  node in place, and the DOM's replace-data steps collapse any range whose
 *  endpoints sit inside that node. Node identity never enters into it, so
 *  keeping nodes mounted across a frame (#3830) is not enough on its own:
 *  repainting the text under the finger destroys the selection, and on iOS
 *  that dismisses the Copy callout mid-gesture. Freezing the paint for the
 *  length of the gesture is the terminal convention, tmux copy-mode included.
 */
export function useSelectionHold<T>(
  value: T,
  containerRef: RefObject<HTMLElement | null>,
  /** Fold context that only became reachable after the hold began into the
   *  held value, so it is frozen from then on like everything else behind the
   *  hold. Must return null once there is nothing left to fold: each fold
   *  re-renders, and a callback that always returns a value will not settle. */
  absorb?: (held: T, next: T) => T | null,
): { value: T; held: boolean } {
  const subscribe = useCallback((onChange: () => void) => {
    document.addEventListener("selectionchange", onChange);
    return () => document.removeEventListener("selectionchange", onChange);
  }, []);
  // Read through useSyncExternalStore rather than from a selectionchange
  // handler so the answer is re-derived during the render an arriving value
  // triggers. A handler's state update could still be queued at that point,
  // and that render would repaint over a selection made moments earlier.
  // Either endpoint inside the container counts, so a drag that ends outside
  // it is held too.
  const getSnapshot = useCallback(() => {
    const container = containerRef.current;
    const selection = document.getSelection();
    if (!container || !selection || selection.isCollapsed || selection.rangeCount === 0) return false;
    return container.contains(selection.anchorNode) || container.contains(selection.focusNode);
  }, [containerRef]);
  const selecting = useSyncExternalStore(subscribe, getSnapshot, () => false);

  // The last committed value. Trails `shown`, so at the render that first
  // sees a selection it is the value the user selected against, even when
  // the selection and a new value land in the same render.
  const painted = useRef(value);
  const [held, setHeld] = useState<{ value: T } | null>(null);
  if (selecting) {
    // Adjust-state-during-render, as elsewhere in this tree. Reading the ref
    // here is safe and deliberate: it is written only in the layout effect
    // below, so it holds the same committed value for every pass of a render.
    // Mirroring it in state instead costs a second render pass per streamed
    // value, on the path this component is built to keep cheap.
    // eslint-disable-next-line react-hooks/refs
    if (held === null) setHeld({ value: painted.current });
    else {
      const absorbed = absorb?.(held.value, value) ?? null;
      if (absorbed !== null) setHeld({ value: absorbed });
    }
  } else if (held !== null) {
    setHeld(null);
  }
  const shown = selecting && held ? held.value : value;
  useLayoutEffect(() => {
    painted.current = shown;
  });
  return { value: shown, held: selecting && held !== null };
}

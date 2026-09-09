import { useCallback, useState, useSyncExternalStore } from "react";
import type { RefObject } from "react";

/** Holds the rendered value still while the user holds a text selection
 *  inside `containerRef`, returning the value that was on screen when the
 *  selection began.
 *
 *  React updates a row whose text changed by rewriting its existing text
 *  node in place, and the DOM's replace-data steps collapse any range whose
 *  endpoints sit inside that node. Keeping row nodes mounted (#3830) is
 *  therefore not enough: an agent that repaints the row under the finger
 *  still destroys the selection, and on iOS that dismisses the Copy callout
 *  mid-gesture. A full-screen agent repaints every row of every frame, so
 *  there a selection never survives long enough to copy.
 *
 *  Freezing the paint for the length of the gesture is the terminal
 *  convention (tmux copy-mode does the same). The stream itself is
 *  untouched: the agent keeps running and the view catches up on release.
 */
export function useSelectionHold<T>(value: T, containerRef: RefObject<HTMLElement | null>): T {
  const subscribe = useCallback((onChange: () => void) => {
    document.addEventListener("selectionchange", onChange);
    return () => document.removeEventListener("selectionchange", onChange);
  }, []);
  // Read through useSyncExternalStore rather than from a selectionchange
  // handler so the answer is re-derived during the render an arriving frame
  // triggers. A handler's state update could still be queued at that point,
  // and the frame would repaint over a selection made moments earlier.
  const getSnapshot = useCallback(() => {
    const container = containerRef.current;
    const selection = document.getSelection();
    if (!container || !selection || selection.isCollapsed || selection.rangeCount === 0) return false;
    return container.contains(selection.anchorNode) && container.contains(selection.focusNode);
  }, [containerRef]);
  const selecting = useSyncExternalStore(subscribe, getSnapshot, () => false);

  // Adjust-state-during-render, as elsewhere in this tree. `painted` trails
  // the last value rendered without a selection, so it is the frame the user
  // selected against even when the selection and a new frame land together.
  // The wrapper object distinguishes holding a null frame from not holding.
  const [painted, setPainted] = useState(value);
  const [held, setHeld] = useState<{ value: T } | null>(null);
  if (selecting) {
    if (held === null) setHeld({ value: painted });
  } else {
    if (held !== null) setHeld(null);
    if (painted !== value) setPainted(value);
  }
  return selecting && held ? held.value : value;
}

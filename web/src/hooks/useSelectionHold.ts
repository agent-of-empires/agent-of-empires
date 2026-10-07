import { useCallback, useLayoutEffect, useRef, useState, useSyncExternalStore } from "react";
import type { RefObject } from "react";
import { listen } from "./domEvents";

// Repainting text under a selection collapses it (and dismisses the iOS Copy callout), so hold the value.
export function useSelectionHold<T>(
  value: T,
  containerRef: RefObject<HTMLElement | null>,
  absorb?: (held: T, next: T) => T | null,
): { value: T; held: boolean } {
  const subscribe = useCallback((onChange: () => void) => listen(onChange, [document, "selectionchange"]), []);
  const getSnapshot = useCallback(() => {
    const container = containerRef.current;
    const selection = document.getSelection();
    if (!container || !selection || selection.isCollapsed || selection.rangeCount === 0) return false;
    return container.contains(selection.anchorNode) || container.contains(selection.focusNode);
  }, [containerRef]);
  const selecting = useSyncExternalStore(subscribe, getSnapshot, () => false);

  const painted = useRef(value);
  const [held, setHeld] = useState<{ value: T } | null>(null);
  if (selecting) {
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

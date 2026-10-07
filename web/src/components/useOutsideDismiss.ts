import { useEffect, type RefObject } from "react";

/** While `active`, a mousedown outside every ref or an Escape keydown calls `onDismiss`. */
export function useOutsideDismiss(active: boolean, refs: RefObject<HTMLElement | null>[], onDismiss: () => void) {
  useEffect(() => {
    if (!active) return;
    const onMouseDown = (e: MouseEvent) => {
      if (refs.some((r) => r.current?.contains(e.target as Node))) return;
      onDismiss();
    };
    const onKeydown = (e: KeyboardEvent) => {
      if (e.key === "Escape") onDismiss();
    };
    document.addEventListener("mousedown", onMouseDown);
    document.addEventListener("keydown", onKeydown);
    return () => {
      document.removeEventListener("mousedown", onMouseDown);
      document.removeEventListener("keydown", onKeydown);
    };
    // Refs are stable containers; `onDismiss` identity changes do not need a resubscribe.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active]);
}

import { useEffect, type RefObject } from "react";

/**
 * While `active`, a mousedown outside every ref or an Escape keydown calls `onDismiss`. The Escape is
 * marked handled (`defaultPrevented`), so window-level Escape handlers, such as clearing the sidebar
 * selection, leave it to this popup. `onDismiss` gets the event, so a caller can restore focus after Escape only.
 */
export function useOutsideDismiss(
  active: boolean,
  refs: RefObject<HTMLElement | null>[],
  onDismiss: (e: MouseEvent | KeyboardEvent) => void,
) {
  useEffect(() => {
    if (!active) return;
    const onMouseDown = (e: MouseEvent) => {
      if (refs.some((r) => r.current?.contains(e.target as Node))) return;
      onDismiss(e);
    };
    const onKeydown = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      onDismiss(e);
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

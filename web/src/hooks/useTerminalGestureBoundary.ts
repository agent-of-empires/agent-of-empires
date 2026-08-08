import { useEffect, useLayoutEffect, useRef } from "react";
import type { RefObject } from "react";

// Owns the boundary between an alternate-screen terminal gesture and the
// browser viewport. Rendering code can use the returned refs for forwarding,
// while this hook guarantees that the browser never promotes that gesture into
// a page pan.
export function useTerminalGestureBoundary({
  scrollerRef,
  forwardMode,
  mouseSgr,
}: {
  scrollerRef: RefObject<HTMLDivElement | null>;
  forwardMode: boolean;
  mouseSgr: boolean;
}) {
  const forwardModeRef = useRef(forwardMode);
  const mouseSgrRef = useRef(mouseSgr);

  // A mode frame can land while a finger is down. Publish it before paint so
  // the native touch listener below owns the next move, not the document.
  useLayoutEffect(() => {
    forwardModeRef.current = forwardMode;
    mouseSgrRef.current = mouseSgr;
  }, [forwardMode, mouseSgr]);

  useEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    const stopPagePan = (event: TouchEvent) => {
      if (forwardModeRef.current && event.cancelable) event.preventDefault();
    };
    el.addEventListener("touchmove", stopPagePan, { passive: false });
    return () => el.removeEventListener("touchmove", stopPagePan);
  }, [scrollerRef]);

  return { forwardModeRef, mouseSgrRef };
}

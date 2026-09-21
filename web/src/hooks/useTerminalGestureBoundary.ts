import { useEffect, useLayoutEffect, useRef } from "react";
import type { RefObject } from "react";

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

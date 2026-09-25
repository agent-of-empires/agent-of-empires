import { useEffect, useLayoutEffect, useRef } from "react";
import type { RefObject } from "react";

export function useTerminalGestureBoundary({
  scrollerRef,
  forwardMode,
  mouseTracking,
  mouseSgr,
}: {
  scrollerRef: RefObject<HTMLDivElement | null>;
  /** The pane owns the gesture: it is on the alternate screen, which has no
   *  capturable scrollback for the browser to scroll instead. */
  forwardMode: boolean;
  /** The app asked for mouse reports. The wheel does not need this (the
   *  daemon sends PageUp/PageDown to a full-screen app without it), but a
   *  button press does: an app that never enabled tracking would read the
   *  report as typed escape bytes. */
  mouseTracking: boolean;
  mouseSgr: boolean;
}) {
  const forwardModeRef = useRef(forwardMode);
  const mouseTrackingRef = useRef(mouseTracking);
  const mouseSgrRef = useRef(mouseSgr);

  useLayoutEffect(() => {
    forwardModeRef.current = forwardMode;
    mouseTrackingRef.current = mouseTracking;
    mouseSgrRef.current = mouseSgr;
  }, [forwardMode, mouseTracking, mouseSgr]);

  useEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    const stopPagePan = (event: TouchEvent) => {
      if (forwardModeRef.current && event.cancelable) event.preventDefault();
    };
    el.addEventListener("touchmove", stopPagePan, { passive: false });
    return () => el.removeEventListener("touchmove", stopPagePan);
  }, [scrollerRef]);

  return { forwardModeRef, mouseTrackingRef, mouseSgrRef };
}

import { useCallback, useEffect, useRef } from "react";
import { useSnapshotStore } from "./useSnapshotStore";
import { listen } from "./domEvents";

interface MobileKeyboardSnapshot {
  isMobile: boolean;
  keyboardOpen: boolean;
  keyboardHeight: number;
}

export function useMobileKeyboard() {
  const { state, setState } = useSnapshotStore<MobileKeyboardSnapshot>(() => ({
    isMobile: typeof window !== "undefined" && !!window.matchMedia?.("(pointer: coarse)").matches,
    keyboardOpen: false,
    keyboardHeight: 0,
  }));
  const update = useCallback(
    (partial: Partial<MobileKeyboardSnapshot>) => setState((prev) => ({ ...prev, ...partial })),
    [setState],
  );

  const rafRef = useRef(0);
  const stableCountRef = useRef(0);
  const lastOcclusionRef = useRef(0);
  const fullHeightRef = useRef(0);

  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const mql = window.matchMedia("(pointer: coarse)");
    const onChange = () =>
      update(mql.matches ? { isMobile: true } : { isMobile: false, keyboardOpen: false, keyboardHeight: 0 });
    mql.addEventListener?.("change", onChange);
    return () => mql.removeEventListener?.("change", onChange);
  }, [update]);

  useEffect(() => {
    if (!state.isMobile) return;
    const vv = window.visualViewport;
    if (!vv) return;

    fullHeightRef.current = Math.max(window.innerHeight, vv.height);

    let lastOpen = false;
    let lastPadding = 0;

    const safeBottom =
      parseFloat(getComputedStyle(document.documentElement).getPropertyValue("--safe-area-bottom")) || 0;

    const measure = () => {
      const currentVvH = vv.height;

      if (currentVvH > fullHeightRef.current - 50) {
        fullHeightRef.current = Math.max(fullHeightRef.current, currentVvH);
      }

      const totalOcclusion = fullHeightRef.current - currentVvH;
      const open = totalOcclusion > 100;

      const padding = open ? Math.max(0, window.innerHeight - currentVvH - safeBottom) : 0;

      if (open !== lastOpen || padding !== lastPadding) {
        lastOpen = open;
        lastPadding = padding;
        stableCountRef.current = 0;
        update({ keyboardOpen: open, keyboardHeight: padding });
      }

      return totalOcclusion;
    };

    const MAX_POLL_FRAMES = 20;
    const STABLE_THRESHOLD = 3;
    const startPolling = () => {
      cancelAnimationFrame(rafRef.current);
      stableCountRef.current = 0;
      let frameCount = 0;
      const poll = () => {
        frameCount++;
        const occlusion = measure();
        if (Math.abs(occlusion - lastOcclusionRef.current) < 1) {
          stableCountRef.current++;
        } else {
          stableCountRef.current = 0;
        }
        lastOcclusionRef.current = occlusion;
        if (stableCountRef.current < STABLE_THRESHOLD && frameCount < MAX_POLL_FRAMES) {
          rafRef.current = requestAnimationFrame(poll);
        }
      };
      rafRef.current = requestAnimationFrame(poll);
    };

    const handleViewportChange = () => {
      measure();
      startPolling();
    };

    const handleFocusIn = (e: Event) => {
      const tag = (e.target as HTMLElement | null)?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") {
        startPolling();
      }
    };

    let orientTimer: ReturnType<typeof setTimeout> | null = null;
    const handleOrientationChange = () => {
      fullHeightRef.current = 0;
      if (orientTimer) clearTimeout(orientTimer);
      orientTimer = setTimeout(() => {
        fullHeightRef.current = Math.max(window.innerHeight, vv.height);
        measure();
      }, 500);
    };

    measure();
    const stop = [
      listen(handleViewportChange, [vv, "resize"], [vv, "scroll"], [window, "scroll"]),
      listen(handleFocusIn, [document, "focusin"]),
      listen(handleOrientationChange, [window, "orientationchange"]),
    ];
    return () => {
      cancelAnimationFrame(rafRef.current);
      if (orientTimer) clearTimeout(orientTimer);
      for (const off of stop) off();
    };
  }, [state.isMobile, update]);

  return state;
}

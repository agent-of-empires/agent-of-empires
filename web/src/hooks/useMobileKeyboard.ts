import { useEffect, useRef, useState, useSyncExternalStore } from "react";

// Tracks soft-keyboard state on touch devices via visualViewport.
//
// keyboardOpen flips as soon as the visual viewport is occluded enough to
// be a keyboard (not a URL bar nudge). The structured-view composer uses
// it for layout posture. (Terminal surfaces derive their open/closed
// state from input focus instead, which is exact.)
//
// keyboardHeight is the bottom inset needed to keep content above the
// keyboard on iOS regular Safari, the one platform where the layout
// viewport does not shrink with the keyboard; it stays 0 on iOS PWA /
// iOS 26 Safari / Android Chrome, where `100dvh` shrinks natively and
// the flex layout already accounts for it.
//
// measure() also snaps back any stray layout-viewport scroll: iOS
// scrolls the page to reveal a focused input even under overflow:hidden
// roots, and nothing else ever scrolls the layout viewport.
//
// The PTY-era machinery (debounced keyboardOcclusion, the
// stableViewportHeight root pin) is gone: every mobile terminal surface
// renders the capture-snapshot live view now, so no PTY needs shielding
// from keyboard-driven layout changes.

interface MobileKeyboardSnapshot {
  isMobile: boolean;
  keyboardOpen: boolean;
  keyboardHeight: number;
}

function createKeyboardStore() {
  const initialIsMobile = typeof window !== "undefined" && window.matchMedia?.("(pointer: coarse)").matches;
  let snapshot: MobileKeyboardSnapshot = {
    isMobile: initialIsMobile,
    keyboardOpen: false,
    keyboardHeight: 0,
  };
  const listeners = new Set<() => void>();
  return {
    getSnapshot: () => snapshot,
    update: (partial: Partial<MobileKeyboardSnapshot>) => {
      snapshot = { ...snapshot, ...partial };
      listeners.forEach((l) => l());
    },
    subscribe: (listener: () => void) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

type KeyboardStore = ReturnType<typeof createKeyboardStore>;

export function useMobileKeyboard() {
  const [store] = useState<KeyboardStore>(() => createKeyboardStore());
  const state = useSyncExternalStore(store.subscribe, store.getSnapshot);

  const rafRef = useRef(0);
  const stableCountRef = useRef(0);
  const lastOcclusionRef = useRef(0);
  const fullHeightRef = useRef(0);

  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const mql = window.matchMedia("(pointer: coarse)");
    const onChange = () => {
      if (mql.matches) {
        store.update({ isMobile: true });
      } else {
        // Leaving mobile mode: clear any keyboard metrics so stale padding
        // from a prior keyboard session can't survive on a now-desktop layout.
        store.update({
          isMobile: false,
          keyboardOpen: false,
          keyboardHeight: 0,
        });
      }
    };
    mql.addEventListener?.("change", onChange);
    return () => mql.removeEventListener?.("change", onChange);
  }, [store]);

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
      // iOS scrolls the layout viewport to reveal a focused input even
      // though the app root is overflow:hidden (the xterm helper
      // textarea rides the terminal cursor near the bottom of the pane,
      // so opening the keyboard shoves the whole app up by roughly the
      // keyboard height and it never comes back). The app never scrolls
      // the layout viewport itself, so any non-zero scroll here is
      // WebKit's doing; snap it back so occlusion padding stays the only
      // thing that moves the terminal.
      if (window.scrollY !== 0 || document.documentElement.scrollTop !== 0) {
        window.scrollTo(0, 0);
      }
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
        store.update({ keyboardOpen: open, keyboardHeight: padding });
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

    const handleFocusIn = (e: FocusEvent) => {
      const tag = (e.target as HTMLElement)?.tagName;
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
    vv.addEventListener("resize", handleViewportChange);
    vv.addEventListener("scroll", handleViewportChange);
    document.addEventListener("focusin", handleFocusIn);
    window.addEventListener("orientationchange", handleOrientationChange);
    // WebKit's focus-driven layout-viewport scroll does not always move
    // the visual viewport relative to the layout viewport, so the vv
    // "scroll" listener alone can miss it; the window scroll event is
    // the reliable signal for the snap-back in measure().
    window.addEventListener("scroll", handleViewportChange);
    return () => {
      cancelAnimationFrame(rafRef.current);
      if (orientTimer) clearTimeout(orientTimer);
      vv.removeEventListener("resize", handleViewportChange);
      vv.removeEventListener("scroll", handleViewportChange);
      document.removeEventListener("focusin", handleFocusIn);
      window.removeEventListener("orientationchange", handleOrientationChange);
      window.removeEventListener("scroll", handleViewportChange);
    };
  }, [state.isMobile, store]);

  return {
    isMobile: state.isMobile,
    keyboardOpen: state.keyboardOpen,
    keyboardHeight: state.keyboardHeight,
  };
}

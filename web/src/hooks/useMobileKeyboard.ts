import { useEffect, useRef, useState, useSyncExternalStore } from "react";

// Detects touch-primary devices and tracks soft-keyboard state via visualViewport.
// isMobile is used to decide whether the mobile toolbar renders at all.
//
// keyboardOpen flips as soon as the visual viewport is occluded enough to be
// a keyboard (not a URL bar nudge). It drives icon/affordance state and is
// allowed to update live; it does not by itself resize the main terminal.
//
// keyboardHeight is the extra padding needed to keep content above the keyboard
// for iOS regular Safari (where the layout viewport doesn't shrink); it stays
// 0 on iOS PWA and iOS 26 Safari, where innerHeight shrinks with the keyboard
// and the flex layout would already account for it. RightPanel's paired
// terminal uses this live value directly.
//
// keyboardOcclusion is the live, cross-platform height the soft keyboard is
// covering: stableFullHeight - visualViewport.height. It is the value the main
// TerminalView pads its layout by so the terminal pane shrinks while the
// keyboard is up and grows back when it dismisses. Unlike keyboardHeight, it
// stays correct on iOS PWA / iOS 26 Safari / Android Chrome, where innerHeight
// shrinks WITH the keyboard. The commit is debounced so the ~300ms keyboard
// animation, which ramps the occlusion frame by frame, produces a single PTY
// resize per open/close instead of a storm.
//
// stableViewportHeight is the largest window.innerHeight seen since the last
// orientation change. On iOS PWA / iOS 26 Safari / Android Chrome, innerHeight
// shrinks when the keyboard opens and the App root's `100dvh` would shrink
// with it; the App root applies this as an explicit pixel height instead so
// the layout stays at the no-keyboard size and occlusion padding (not a
// shrinking root) is what moves the terminal. Reset on orientation change.
const OCCLUSION_COMMIT_DEBOUNCE_MS = 150;

interface MobileKeyboardSnapshot {
  isMobile: boolean;
  keyboardOpen: boolean;
  keyboardHeight: number;
  keyboardOcclusion: number;
  stableViewportHeight: number;
}

function createKeyboardStore() {
  const initialIsMobile =
    typeof window !== "undefined" &&
    window.matchMedia?.("(pointer: coarse)").matches;
  let snapshot: MobileKeyboardSnapshot = {
    isMobile: initialIsMobile,
    keyboardOpen: false,
    keyboardHeight: 0,
    keyboardOcclusion: 0,
    stableViewportHeight: 0,
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
  const committedOcclusionRef = useRef(0);
  const commitTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
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
          keyboardOcclusion: 0,
          stableViewportHeight: 0,
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

    const safeBottom = parseFloat(
      getComputedStyle(document.documentElement)
        .getPropertyValue("--safe-area-bottom"),
    ) || 0;

    const scheduleOcclusionCommit = (target: number) => {
      if (target === committedOcclusionRef.current) return;
      if (commitTimerRef.current) clearTimeout(commitTimerRef.current);
      commitTimerRef.current = setTimeout(() => {
        committedOcclusionRef.current = target;
        store.update({ keyboardOcclusion: target });
      }, OCCLUSION_COMMIT_DEBOUNCE_MS);
    };

    const measure = () => {
      const currentVvH = vv.height;

      if (currentVvH > fullHeightRef.current - 50) {
        fullHeightRef.current = Math.max(fullHeightRef.current, currentVvH);
      }

      const totalOcclusion = fullHeightRef.current - currentVvH;
      const open = totalOcclusion > 100;

      const padding = open
        ? Math.max(0, window.innerHeight - currentVvH - safeBottom)
        : 0;

      if (open !== lastOpen || padding !== lastPadding) {
        lastOpen = open;
        lastPadding = padding;
        stableCountRef.current = 0;
        store.update({ keyboardOpen: open, keyboardHeight: padding });
      }

      scheduleOcclusionCommit(open ? Math.max(0, totalOcclusion) : 0);

      const heightCandidate = Math.max(window.innerHeight, currentVvH);
      if (heightCandidate > store.getSnapshot().stableViewportHeight) {
        store.update({ stableViewportHeight: heightCandidate });
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
      store.update({ stableViewportHeight: 0 });
      if (commitTimerRef.current) clearTimeout(commitTimerRef.current);
      committedOcclusionRef.current = 0;
      store.update({ keyboardOcclusion: 0 });
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
    return () => {
      cancelAnimationFrame(rafRef.current);
      if (orientTimer) clearTimeout(orientTimer);
      if (commitTimerRef.current) clearTimeout(commitTimerRef.current);
      vv.removeEventListener("resize", handleViewportChange);
      vv.removeEventListener("scroll", handleViewportChange);
      document.removeEventListener("focusin", handleFocusIn);
      window.removeEventListener("orientationchange", handleOrientationChange);
    };
  }, [state.isMobile, store]);

  return {
    isMobile: state.isMobile,
    keyboardOpen: state.keyboardOpen,
    keyboardHeight: state.keyboardHeight,
    keyboardOcclusion: state.keyboardOcclusion,
    stableViewportHeight: state.stableViewportHeight,
  };
}

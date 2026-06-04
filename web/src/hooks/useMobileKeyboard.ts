import { useEffect, useRef, useState } from "react";

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

export function useMobileKeyboard() {
  const [isMobile, setIsMobile] = useState(() =>
    typeof window !== "undefined" &&
    window.matchMedia?.("(pointer: coarse)").matches,
  );
  const [keyboardOpen, setKeyboardOpen] = useState(false);
  const [keyboardHeight, setKeyboardHeight] = useState(0);
  const [keyboardOcclusion, setKeyboardOcclusion] = useState(0);
  const [stableViewportHeight, setStableViewportHeight] = useState(0);
  const rafRef = useRef(0);
  const stableCountRef = useRef(0);
  const lastOcclusionRef = useRef(0);
  // Latest occlusion target already committed to React state. The debounce
  // only schedules a commit when the target actually changes.
  const committedOcclusionRef = useRef(0);
  const commitTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Track the max viewport height seen (before keyboard opens) so we can
  // detect keyboard-open even when innerHeight shrinks with the keyboard.
  const fullHeightRef = useRef(0);

  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const mql = window.matchMedia("(pointer: coarse)");
    const onChange = () => setIsMobile(mql.matches);
    mql.addEventListener?.("change", onChange);
    return () => mql.removeEventListener?.("change", onChange);
  }, []);

  useEffect(() => {
    if (!isMobile) return;
    const vv = window.visualViewport;
    if (!vv) return;

    fullHeightRef.current = Math.max(window.innerHeight, vv.height);

    let lastOpen = false;
    let lastPadding = 0;

    // Read the bottom safe-area inset once. The App root applies this as
    // padding, so the keyboard compensation should not include it.
    const safeBottom = parseFloat(
      getComputedStyle(document.documentElement)
        .getPropertyValue("--safe-area-bottom"),
    ) || 0;

    // Commit the occlusion to React state, but only after it stops changing
    // for OCCLUSION_COMMIT_DEBOUNCE_MS. The keyboard animation ramps the
    // occlusion over several frames; committing each frame would SIGWINCH the
    // PTY repeatedly for one open/close. Debouncing collapses it to one.
    const scheduleOcclusionCommit = (target: number) => {
      if (target === committedOcclusionRef.current) return;
      if (commitTimerRef.current) clearTimeout(commitTimerRef.current);
      commitTimerRef.current = setTimeout(() => {
        committedOcclusionRef.current = target;
        setKeyboardOcclusion(target);
      }, OCCLUSION_COMMIT_DEBOUNCE_MS);
    };

    const measure = () => {
      const currentVvH = vv.height;

      // Update the full height when viewport grows (keyboard closed,
      // orientation change, etc.).
      if (currentVvH > fullHeightRef.current - 50) {
        fullHeightRef.current = Math.max(fullHeightRef.current, currentVvH);
      }

      // Detect keyboard open: significant drop from remembered full height.
      const totalOcclusion = fullHeightRef.current - currentVvH;
      const open = totalOcclusion > 100;

      // keyboardHeight: the gap between innerHeight and the visual viewport,
      // minus the bottom safe area the App root already handles. When
      // innerHeight shrinks with the keyboard (iOS PWA, iOS 26 Safari),
      // innerHeight ≈ vvHeight and this is ≈ 0; RightPanel consumes it live.
      const padding = open
        ? Math.max(0, window.innerHeight - currentVvH - safeBottom)
        : 0;

      if (open !== lastOpen || padding !== lastPadding) {
        lastOpen = open;
        lastPadding = padding;
        stableCountRef.current = 0;
        setKeyboardOpen(open);
        setKeyboardHeight(padding);
      }

      // totalOcclusion is the true keyboard size on every platform (it is
      // measured against the remembered full height, not innerHeight, so it
      // stays correct where innerHeight shrinks with the keyboard). The main
      // terminal pads by it while open and releases to 0 while closed.
      scheduleOcclusionCommit(open ? Math.max(0, totalOcclusion) : 0);

      // Latch the max layout-viewport height. On iOS PWA the keyboard
      // shrinks innerHeight, so without this 100dvh would also shrink and
      // resize the terminal container. App.tsx pins the root to this value.
      // Take the larger of innerHeight and vv.height so a mount that
      // happens to find the keyboard already open (innerHeight reduced)
      // can still latch to vv.height if that's somehow larger; in
      // practice both match in the no-keyboard state and that's what we
      // capture on first measure.
      const heightCandidate = Math.max(window.innerHeight, currentVvH);
      setStableViewportHeight((prev) =>
        heightCandidate > prev ? heightCandidate : prev,
      );
      return totalOcclusion;
    };

    // iOS keyboard animation takes ~300ms but visualViewport events don't
    // fire every frame during it. Poll via rAF to catch the transition,
    // stopping early when the measurement stabilizes (same value 3 frames
    // in a row) or after 20 frames max to avoid burning CPU while typing.
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

    // Also poll briefly when any focusin happens; keyboard may be about
    // to open but visualViewport hasn't started updating yet.
    const handleFocusIn = (e: FocusEvent) => {
      const tag = (e.target as HTMLElement)?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") {
        startPolling();
      }
    };

    // Orientation changes reset the full height baseline: the keyboard
    // physically swaps shape between portrait and landscape, so the stale
    // baseline would mis-measure occlusion until the next full measure.
    let orientTimer: ReturnType<typeof setTimeout> | null = null;
    const handleOrientationChange = () => {
      fullHeightRef.current = 0;
      setStableViewportHeight(0);
      if (commitTimerRef.current) clearTimeout(commitTimerRef.current);
      committedOcclusionRef.current = 0;
      setKeyboardOcclusion(0);
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
  }, [isMobile]);

  return {
    isMobile,
    keyboardOpen,
    keyboardHeight,
    keyboardOcclusion,
    stableViewportHeight,
  };
}

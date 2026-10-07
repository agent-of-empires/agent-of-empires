// Shared side-effect-free platform probes.

/** Misses iPadOS in desktop mode, which reports a Mac userAgent; callers only need iPhone. */
export const isIOS = (): boolean => typeof navigator !== "undefined" && /iPad|iPhone|iPod/.test(navigator.userAgent);

export const isStandalone = (): boolean => {
  if (typeof window === "undefined") return false;
  const ios = (window.navigator as unknown as { standalone?: boolean }).standalone === true;
  const displayMode = window.matchMedia?.("(display-mode: standalone)").matches;
  return ios || !!displayMode;
};

/** A precise hovering pointer, taken as a sign of a physical keyboard. */
export const hasFinePointer = (): boolean =>
  typeof window !== "undefined" && !!window.matchMedia?.("(hover: hover) and (pointer: fine)").matches;

// Small, side-effect-free platform probes shared across the dashboard.
// Kept here (rather than duplicated per hook) so iOS / installed-PWA
// detection reads the same way everywhere it matters (push support,
// keyboard layout quirks, etc.).

/** True on iPhone / iPad / iPod Safari or an installed iOS PWA. iPadOS 13+
 *  reports a Mac userAgent, so this misses iPad-in-desktop-mode; every caller
 *  today only needs the iPhone case, which always carries "iPhone". */
export const isIOS = (): boolean => typeof navigator !== "undefined" && /iPad|iPhone|iPod/.test(navigator.userAgent);

/** True when running as an installed PWA (Add to Home Screen / standalone
 *  display mode) rather than inside a browser tab. iOS exposes
 *  `navigator.standalone`; other platforms use the display-mode media query. */
export const isStandalone = (): boolean => {
  if (typeof window === "undefined") return false;
  const ios = (window.navigator as unknown as { standalone?: boolean }).standalone === true;
  const displayMode = window.matchMedia?.("(display-mode: standalone)").matches;
  return ios || !!displayMode;
};

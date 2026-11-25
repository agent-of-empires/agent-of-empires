// Shared side-effect-free platform probes.

/** iPhone, iPod or iPad, including iPadOS in desktop mode: a Mac userAgent with touch points, which a real Mac lacks. */
export const isIOS = (): boolean =>
  typeof navigator !== "undefined" &&
  (/iPad|iPhone|iPod/.test(navigator.userAgent) ||
    (/Macintosh/.test(navigator.userAgent) && navigator.maxTouchPoints > 1));

export const isStandalone = (): boolean => {
  if (typeof window === "undefined") return false;
  const ios = (window.navigator as unknown as { standalone?: boolean }).standalone === true;
  const displayMode = window.matchMedia?.("(display-mode: standalone)").matches;
  return ios || !!displayMode;
};

/** A precise hovering pointer, taken as a sign of a physical keyboard. */
export const hasFinePointer = (): boolean =>
  typeof window !== "undefined" && !!window.matchMedia?.("(hover: hover) and (pointer: fine)").matches;

/** Push and install need a secure context; plain http counts only on loopback. */
export const isSecureOrigin = (): boolean => {
  if (typeof window === "undefined") return false;
  if (window.isSecureContext) return true;
  const host = window.location.hostname;
  return host === "localhost" || host === "127.0.0.1" || host === "[::1]";
};

export const supportsPush = (): boolean =>
  typeof window !== "undefined" && "serviceWorker" in navigator && "PushManager" in window && "Notification" in window;

/** `ios` installs to the Home Screen, `push` turns notifications on in this tab, `other` installs from the browser's menu. */
export type InstallHint = "ios" | "push" | "other";

/**
 * What a phone or tablet browser tab needs for notifications; null for an installed app, a desktop,
 * or an insecure origin. iOS comes first because its Safari exposes push only to Home Screen apps.
 * Android is a touch device whatever its pointer; anything else needs touch points and no fine
 * pointer. Whether the tab can push is feature-detected, not read from the userAgent.
 */
export function installHint(): InstallHint | null {
  if (isStandalone() || !isSecureOrigin()) return null;
  if (isIOS()) return "ios";
  const touchDevice = /Android/.test(navigator.userAgent) || (!hasFinePointer() && navigator.maxTouchPoints > 0);
  if (!touchDevice) return null;
  return supportsPush() ? "push" : "other";
}

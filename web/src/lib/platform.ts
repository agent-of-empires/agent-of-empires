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

/** Push and install need a secure context; plain http counts only on loopback. */
export const isSecureOrigin = (): boolean => {
  if (typeof window === "undefined") return false;
  if (window.isSecureContext) return true;
  const host = window.location.hostname;
  return host === "localhost" || host === "127.0.0.1" || host === "[::1]";
};

export type InstallHint = "ios" | "android" | "other";

/**
 * Which install steps a phone or tablet browser tab needs; null for an installed app, a desktop,
 * or an insecure origin. An iPad with a trackpad has a fine pointer, so it counts as a desktop.
 */
export function installHint(): InstallHint | null {
  if (isStandalone() || hasFinePointer() || !isSecureOrigin()) return null;
  if (isIOS()) return "ios";
  return /Android/.test(navigator.userAgent) ? "android" : "other";
}

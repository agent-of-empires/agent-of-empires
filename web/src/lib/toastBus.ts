export type ToastKind = "error" | "info";

export interface ToastApi {
  push: (message: string, kind?: ToastKind) => void;
  error: (message: string) => void;
  info: (message: string) => void;
  /** A click-to-open toast: tapping it opens `href` in a new tab. Used for a
   *  worker `ui.open_url`, which cannot `window.open` from an async push without
   *  the popup blocker, so the open waits for the user's click. */
  openLink: (message: string, href: string) => void;
}

interface ToastBus {
  handler: ToastApi | null;
}

export const toastBus: ToastBus = { handler: null };

export function reportError(message: string): void {
  toastBus.handler?.error(message);
}

export function reportInfo(message: string): void {
  toastBus.handler?.info(message);
}

export function reportOpenLink(message: string, href: string): void {
  toastBus.handler?.openLink(message, href);
}

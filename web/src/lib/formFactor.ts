// Coarse client form-factor classification for the telemetry seen ping (#1883).
//
// The daemon snapshot's os/arch describe the host running `aoe serve`, not the
// device the user is looking at, so a phone PWA talking to a Mac daemon was
// indistinguishable from a desktop tab. This derives one of a closed set of
// coarse classes from the same media-query primitives the layout hooks already
// use (`usePushSubscription` standalone detection, `useIsCoarsePointer`,
// `useIsWideViewport`), so the seen ping can carry it. No user-agent string,
// screen size, or device model is ever read or sent.

/** The closed set of classes the daemon accepts; anything else is rejected
 *  server-side and never stored. Mirrors `telemetry::form_factor` in Rust. */
export type ClientFormFactor = "desktop" | "desktop_pwa" | "mobile" | "mobile_pwa";

const matchesMedia = (query: string): boolean =>
  typeof window !== "undefined" && Boolean(window.matchMedia?.(query).matches);

/** Installed / standalone PWA: iOS exposes `navigator.standalone`; every other
 *  platform reports `(display-mode: standalone)`. Either counts. */
const isStandalone = (): boolean => {
  if (typeof window === "undefined") return false;
  const ios = (window.navigator as unknown as { standalone?: boolean }).standalone === true;
  return ios || matchesMedia("(display-mode: standalone)");
};

/** Classify the current client. Precedence is documented and deterministic so a
 *  touch laptop or a tablet lands in exactly one bucket:
 *  - `pwa` suffix iff the app is running standalone / installed;
 *  - `mobile` iff the primary pointer is coarse AND the viewport is below the
 *    `md` breakpoint (768px); a wide coarse-pointer client (touch laptop) and a
 *    narrow fine-pointer client (small desktop window) both stay `desktop`;
 *  - `desktop` otherwise. */
export function clientFormFactor(): ClientFormFactor {
  const mobile = matchesMedia("(pointer: coarse)") && !matchesMedia("(min-width: 768px)");
  if (isStandalone()) {
    return mobile ? "mobile_pwa" : "desktop_pwa";
  }
  return mobile ? "mobile" : "desktop";
}

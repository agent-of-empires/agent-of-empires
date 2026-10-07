// Coarse client form factor for the telemetry ping, since the daemon's os/arch describe the host.

import { isStandalone } from "./platform";

/** Mirrors `telemetry::form_factor` in Rust; other values are rejected server-side. */
export type ClientFormFactor = "desktop" | "desktop_pwa" | "mobile" | "mobile_pwa";

const matchesMedia = (query: string): boolean =>
  typeof window !== "undefined" && Boolean(window.matchMedia?.(query).matches);

/** `_pwa` when standalone; `mobile` only for a coarse pointer below 768px; otherwise `desktop`. */
export function clientFormFactor(): ClientFormFactor {
  const mobile = matchesMedia("(pointer: coarse)") && !matchesMedia("(min-width: 768px)");
  if (isStandalone()) {
    return mobile ? "mobile_pwa" : "desktop_pwa";
  }
  return mobile ? "mobile" : "desktop";
}

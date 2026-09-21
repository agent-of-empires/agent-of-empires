// First-run welcome modal policy plus the automated-session guard shared with the tour. The tour-seen flag lives on the backend.
import { safeGetItem, safeSetItem } from "./safeStorage";
import type { TourScope } from "./tourSteps";

// Per-origin storage already separates dev from release.
export const WELCOME_SEEN_KEY = "aoe-welcome-seen";

/** Onboarding overlays would intercept clicks in automated browser sessions. */
export function isAutomatedSession(): boolean {
  return typeof navigator !== "undefined" && navigator.webdriver === true;
}

export function hasSeenWelcome(): boolean {
  return safeGetItem(WELCOME_SEEN_KEY) === "1";
}

export function markWelcomeSeen(): void {
  safeSetItem(WELCOME_SEEN_KEY, "1");
}

/** Settled dashboard, not automated, writable profile, and neither onboarding phase completed. Shown on touch too. */
export function shouldShowWelcome(args: {
  autoLaunchReady: boolean;
  scope: TourScope;
  readOnly: boolean;
  automated: boolean;
  tourSeen: boolean;
  welcomeSeen: boolean;
}): boolean {
  return (
    args.autoLaunchReady &&
    args.scope === "dashboard" &&
    !args.readOnly &&
    !args.automated &&
    !args.tourSeen &&
    !args.welcomeSeen
  );
}

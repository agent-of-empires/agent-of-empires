// First-run onboarding policy and persistence, shared by the theme welcome
// modal (useWelcomePhase) and the tutorial tour (useTour). Kept framework-free
// and pure where possible so the launch decisions are unit-testable without
// React, rAF, or the lazy joyride engine.
//
// Onboarding has two first-run phases with different policies: the theme
// welcome modal (mutates the profile, shown on any pointer, suppressed in
// read-only) runs first; the informational tour (read-only, desktop-only
// auto-launch, replayable from the menu) runs second. Each owns its own seen
// flag so the two never conflate.
import { safeGetItem, safeSetItem } from "./safeStorage";
import type { TourScope } from "./tourSteps";

// Per-origin localStorage already isolates dev (port 8081) from release (8080),
// so flat keys need no app-dir namespace.
export const TOUR_SEEN_KEY = "aoe-tour-seen";
export const WELCOME_SEEN_KEY = "aoe-welcome-seen";

/** Auto-launch and the welcome modal are both suppressed inside automated
 *  browser sessions (a synthetic monitor, a scraper, our Playwright suites):
 *  an onboarding overlay would otherwise intercept clicks in unrelated flows. */
export function isAutomatedSession(): boolean {
  return typeof navigator !== "undefined" && navigator.webdriver === true;
}

export function hasSeenTour(): boolean {
  return safeGetItem(TOUR_SEEN_KEY) === "1";
}

export function markTourSeen(): void {
  safeSetItem(TOUR_SEEN_KEY, "1");
}

export function hasSeenWelcome(): boolean {
  return safeGetItem(WELCOME_SEEN_KEY) === "1";
}

export function markWelcomeSeen(): void {
  safeSetItem(WELCOME_SEEN_KEY, "1");
}

/**
 * Pure decision for the first-run theme welcome modal. Shown only on a settled
 * dashboard, outside automated sessions, when the profile is writable (the
 * modal persists a theme), and only for users who have not yet completed either
 * onboarding phase. The `!tourSeen` clause means users upgrading from before
 * this feature (who already finished the tour) are never re-prompted. Unlike
 * the tour it does not gate on a fine pointer: theme choice is just as relevant
 * on touch, and the modal is responsive.
 */
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

// First-run onboarding policy and persistence for the theme welcome modal
// (useWelcomePhase), plus the shared automated-session guard used by the tour
// too. Kept framework-free and pure where possible so the launch decisions are
// unit-testable without React, rAF, or the lazy joyride engine.
//
// Onboarding has two first-run phases with different policies: the theme
// welcome modal (mutates the profile, shown on any pointer, suppressed in
// read-only) runs first; the informational tour (read-only, desktop-only
// auto-launch, replayable from the menu) runs second. They never conflate: the
// welcome flag lives per-browser here, while the tour-seen flag lives in the
// backend (app_state.has_seen_web_tour, #1832).
import { safeGetItem, safeSetItem } from "./safeStorage";
import type { TourScope } from "./tourSteps";

// Per-origin localStorage already isolates dev (port 8081) from release (8080),
// so a flat key needs no app-dir namespace. The tour-seen flag moved to the
// backend (app_state.has_seen_web_tour) in #1832; only the theme welcome modal
// still persists per-browser here.
export const WELCOME_SEEN_KEY = "aoe-welcome-seen";

/** Auto-launch and the welcome modal are both suppressed inside automated
 *  browser sessions (a synthetic monitor, a scraper, our Playwright suites):
 *  an onboarding overlay would otherwise intercept clicks in unrelated flows. */
export function isAutomatedSession(): boolean {
  return typeof navigator !== "undefined" && navigator.webdriver === true;
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

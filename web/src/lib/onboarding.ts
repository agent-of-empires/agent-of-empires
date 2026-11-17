// Automated-session guard shared by the setup redirect, the tour and the tips auto-pop.

/** Onboarding overlays would intercept clicks in automated browser sessions. */
export function isAutomatedSession(): boolean {
  return typeof navigator !== "undefined" && navigator.webdriver === true;
}

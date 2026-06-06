import { useCallback, useState } from "react";
import {
  hasSeenWelcome,
  isAutomatedSession,
  markWelcomeSeen,
  shouldShowWelcome,
} from "../lib/onboarding";
import type { TourScope } from "../lib/tourSteps";

type Phase = "pending" | "showing" | "done";

export interface UseWelcomePhaseOptions {
  scope: TourScope;
  readOnly: boolean;
  /** The same settled-dashboard gate the tour uses; the welcome decision waits
   *  for it so the modal never flashes over a half-painted dashboard. */
  autoLaunchReady: boolean;
  /** Whether the user has already seen the tour. Since #1832 this lives in the
   *  backend (app_state), so it is passed in rather than read from localStorage:
   *  a veteran who saw the tour on any device must not be re-shown the welcome. */
  tourSeen: boolean;
  /** True once `tourSeen` is resolved (settings fetched). The decision waits for
   *  it so a `false` default cannot show the modal to a veteran before the
   *  backend answer arrives. */
  tourSeenKnown: boolean;
}

export interface UseWelcomePhaseResult {
  showWelcome: boolean;
  /** True once the welcome phase is resolved: either shown and dismissed, or
   *  decided not-applicable. The tour gates its auto-launch on this so the two
   *  first-run phases never overlap. */
  resolved: boolean;
  dismissWelcome: () => void;
}

/**
 * Owns the first-run theme welcome phase: decides once (when the dashboard
 * settles) whether to show the modal, and resolves when it is dismissed or
 * skipped. Leaves the tour's own auto-launch ownership intact; the caller wires
 * `resolved` into the tour's `autoLaunchReady` so the tour follows the modal.
 */
export function useWelcomePhase({
  scope,
  readOnly,
  autoLaunchReady,
  tourSeen,
  tourSeenKnown,
}: UseWelcomePhaseOptions): UseWelcomePhaseResult {
  const [phase, setPhase] = useState<Phase>("pending");

  // Decide exactly once, the first render where the conditions are met.
  if (phase === "pending" && autoLaunchReady && tourSeenKnown) {
    const show = shouldShowWelcome({
      autoLaunchReady,
      scope,
      readOnly,
      automated: isAutomatedSession(),
      tourSeen,
      welcomeSeen: hasSeenWelcome(),
    });
    setPhase(show ? "showing" : "done");
  }

  const dismissWelcome = useCallback(() => {
    markWelcomeSeen();
    setPhase("done");
  }, []);

  return {
    showWelcome: phase === "showing",
    resolved: phase === "done",
    dismissWelcome,
  };
}

import { useCallback, useEffect, useRef, useState } from "react";
import {
  hasSeenTour,
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
}: UseWelcomePhaseOptions): UseWelcomePhaseResult {
  const [phase, setPhase] = useState<Phase>("pending");
  const decidedRef = useRef(false);

  // Decide exactly once, the first frame the dashboard is settled, mirroring
  // the tour's auto-start latch so a later re-render cannot re-open the modal.
  useEffect(() => {
    if (decidedRef.current || !autoLaunchReady) return;
    decidedRef.current = true;
    const show = shouldShowWelcome({
      autoLaunchReady,
      scope,
      readOnly,
      automated: isAutomatedSession(),
      tourSeen: hasSeenTour(),
      welcomeSeen: hasSeenWelcome(),
    });
    setPhase(show ? "showing" : "done");
  }, [autoLaunchReady, scope, readOnly]);

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

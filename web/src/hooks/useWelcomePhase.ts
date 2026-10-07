import { useCallback, useState } from "react";
import { hasSeenWelcome, isAutomatedSession, markWelcomeSeen, shouldShowWelcome } from "../lib/onboarding";
import type { TourScope } from "../lib/tourSteps";

type Phase = "pending" | "showing" | "done";

export interface UseWelcomePhaseOptions {
  scope: TourScope;
  readOnly: boolean;
  autoLaunchReady: boolean;
  tourSeen: boolean;
  tourSeenKnown: boolean;
}

export interface UseWelcomePhaseResult {
  showWelcome: boolean;
  resolved: boolean;
  dismissWelcome: () => void;
}

export function useWelcomePhase({
  scope,
  readOnly,
  autoLaunchReady,
  tourSeen,
  tourSeenKnown,
}: UseWelcomePhaseOptions): UseWelcomePhaseResult {
  const [phase, setPhase] = useState<Phase>("pending");

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

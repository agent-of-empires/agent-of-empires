// First-run flows in order: theme welcome, then the tour, then tip-of-the-day auto-pop.

import { useCallback, useEffect, useRef, useState } from "react";
import { fetchSettings, markWebTourSeen } from "../../lib/api";
import { isAutomatedSession } from "../../lib/onboarding";
import { safeGetItem, safeRemoveItem } from "../../lib/safeStorage";
import type { TourScope } from "../../lib/tourSteps";
import { shouldAutoPopTips, type UseTipsResult } from "../useTips";
import { useTour, type UseTourOptions } from "../useTour";
import { useWelcomePhase } from "../useWelcomePhase";

const LEGACY_TOUR_SEEN_KEY = "aoe-tour-seen";

interface Options {
  scope: TourScope;
  readOnly: boolean;
  cityhall: boolean;
  isDesktop: boolean;
  autoLaunchReady: boolean;
  tips: UseTipsResult;
  telemetryPending: boolean;
  onNavigate: UseTourOptions["onNavigate"];
}

export function useOnboarding({
  scope,
  readOnly,
  cityhall,
  isDesktop,
  autoLaunchReady,
  tips,
  telemetryPending,
  onNavigate,
}: Options) {
  const [tourSeen, setTourSeen] = useState(false);
  const [tourSeenKnown, setTourSeenKnown] = useState(false);
  // Seen state at page load: finishing the tour this session must not pop tips on top of it.
  const tourSeenAtLoadRef = useRef<boolean | null>(null);

  useEffect(() => {
    fetchSettings().then((settings) => {
      if (!settings) return;
      const backendSeen = settings.app_state?.has_seen_web_tour === true;
      const legacySeen = safeGetItem(LEGACY_TOUR_SEEN_KEY) === "1";
      setTourSeen(backendSeen || legacySeen);
      setTourSeenKnown(true);
      tourSeenAtLoadRef.current = backendSeen || legacySeen;
      if (legacySeen && !backendSeen) {
        void markWebTourSeen().then((ok) => {
          if (ok) safeRemoveItem(LEGACY_TOUR_SEEN_KEY);
        });
      }
    });
  }, []);

  const onSeen = useCallback(() => {
    setTourSeen(true);
    void markWebTourSeen();
  }, []);

  const welcome = useWelcomePhase({ scope, readOnly, autoLaunchReady, tourSeen, tourSeenKnown });
  const tour = useTour({
    scope,
    readOnly,
    cityhall,
    isDesktop,
    autoLaunchReady: autoLaunchReady && welcome.resolved,
    seen: tourSeen,
    seenKnown: tourSeenKnown,
    onSeen,
    onNavigate,
  });

  const tipsAutoPoppedRef = useRef(false);
  const tipsAutoPopFrameRef = useRef<number | null>(null);
  useEffect(() => {
    if (tipsAutoPoppedRef.current) return;
    const gate = shouldAutoPopTips({
      loaded: tips.loaded,
      hasUnseen: tips.hasUnseen,
      tourSeenAtLoad: tourSeenAtLoadRef.current,
      onboardingReady: autoLaunchReady && welcome.resolved,
      telemetryPending,
      tourActive: tour.isTourActive,
      automated: isAutomatedSession(),
    });
    if (!gate) return;
    tipsAutoPoppedRef.current = true;
    tipsAutoPopFrameRef.current = requestAnimationFrame(() => tips.open());
  }, [tips, tourSeenKnown, autoLaunchReady, welcome.resolved, telemetryPending, tour.isTourActive]);

  useEffect(
    () => () => {
      if (tipsAutoPopFrameRef.current !== null) cancelAnimationFrame(tipsAutoPopFrameRef.current);
    },
    [],
  );

  return { welcome, tour };
}

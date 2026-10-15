/* eslint-disable react-refresh/only-export-components */
import { lazy, Suspense, useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { resolveTourSteps, type TourScope, type TourStep } from "../lib/tourSteps";
import type { TourSettingsTab } from "../components/tour/TourRunner";
import { isAutomatedSession } from "../lib/onboarding";
import { useLatestRef } from "./useLatestRef";

const TourRunner = lazy(() => import("../components/tour/TourRunner"));

export interface UseTourOptions {
  scope: TourScope;
  readOnly: boolean;
  cityhall: boolean;
  isDesktop: boolean;
  autoLaunchReady: boolean;
  seen: boolean;
  seenKnown: boolean;
  onSeen: () => void;
  onNavigate: (tab: TourSettingsTab | null) => void;
}

export function shouldAutoLaunch(args: {
  autoLaunchReady: boolean;
  seenKnown: boolean;
  scope: TourScope;
  isDesktop: boolean;
  seen: boolean;
  automated: boolean;
}): boolean {
  return (
    args.autoLaunchReady &&
    args.seenKnown &&
    args.scope === "dashboard" &&
    args.isDesktop &&
    !args.seen &&
    !args.automated
  );
}

export interface UseTourResult {
  startTour: () => void;
  isTourActive: boolean;
  tourElement: ReactNode;
}

export function useTour({
  scope,
  readOnly,
  cityhall,
  isDesktop,
  autoLaunchReady,
  seen,
  seenKnown,
  onSeen,
  onNavigate,
}: UseTourOptions): UseTourResult {
  const [run, setRun] = useState(false);
  const [steps, setSteps] = useState<TourStep[]>([]);
  const autoStartedRef = useRef(false);
  const mountedRef = useRef(true);
  const [trackedScope, setTrackedScope] = useState(scope);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  if (trackedScope !== scope) {
    setTrackedScope(scope);
    setRun(false);
  }

  const begin = useCallback(
    (onStarted?: () => void): number => {
      return requestAnimationFrame(() => {
        if (!mountedRef.current) return;
        const resolved = resolveTourSteps({ scope, readOnly, cityhall, isDesktop });
        if (resolved.length === 0) return;
        onStarted?.();
        setSteps(resolved);
        setRun(true);
      });
    },
    [scope, readOnly, cityhall, isDesktop],
  );

  const startTour = useCallback(() => {
    begin();
  }, [begin]);

  const beginRef = useLatestRef(begin);
  useEffect(() => {
    if (autoStartedRef.current) return;
    const automated = isAutomatedSession();
    if (!shouldAutoLaunch({ autoLaunchReady, seenKnown, scope, isDesktop, seen, automated })) return;
    const id = beginRef.current(() => {
      autoStartedRef.current = true;
    });
    return () => cancelAnimationFrame(id);
  }, [autoLaunchReady, seenKnown, scope, isDesktop, seen, beginRef]);

  const handleFinish = useCallback(
    (markSeen: boolean) => {
      setRun(false);
      if (markSeen) onSeen();
    },
    [onSeen],
  );

  const tourElement = run ? (
    <Suspense fallback={null}>
      <TourRunner run={run} steps={steps} onFinish={handleFinish} onNavigate={onNavigate} />
    </Suspense>
  ) : null;

  return { startTour, isTourActive: run, tourElement };
}

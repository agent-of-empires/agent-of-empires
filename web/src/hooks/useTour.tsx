/* eslint-disable react-refresh/only-export-components */
// First-run tutorial controller. Owns the tour's run state, the resolved step
// snapshot, and auto-launch policy. The "seen" flag is now persisted
// server-side (config.toml `app_state.has_seen_web_tour`) so a returning user
// on a new browser or device is not re-shown the tour; App owns the fetch and
// the write and feeds them in as `seen` / `seenKnown` / `onSeen`. The hook is
// engine-agnostic: the react-joyride coupling lives entirely in the lazy
// TourRunner, which is only mounted (and only downloaded) while a tour runs, so
// returning users never pay for the engine. App is the single consumer, so this
// is a plain hook returning the element to render rather than a context.
import { lazy, Suspense, useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { resolveTourSteps, type TourScope, type TourStep } from "../lib/tourSteps";
import { isAutomatedSession } from "../lib/onboarding";

const TourRunner = lazy(() => import("../components/tour/TourRunner"));

export interface UseTourOptions {
  /** Which mutually-exclusive surface is mounted right now. */
  scope: TourScope;
  readOnly: boolean;
  /** Fine pointer. Gates desktop-only steps and suppresses auto-launch on touch. */
  isDesktop: boolean;
  /** True once it is safe to auto-launch: server/about and sessions loaded, the
   *  dashboard is painted, and no blocking overlay is open. */
  autoLaunchReady: boolean;
  /** Whether the user has already seen the tour (from backend app_state, with a
   *  legacy-localStorage fallback during migration). */
  seen: boolean;
  /** True once the seen state is resolved (settings fetched). Auto-launch stays
   *  suppressed until then so a `seen=false` default cannot flash the tour while
   *  the settings request is still in flight. */
  seenKnown: boolean;
  /** Called when the user finishes or skips the tour, so App can persist the
   *  seen flag to the backend. */
  onSeen: () => void;
}

/**
 * Pure auto-launch decision, extracted so the truth table is unit-testable
 * without driving rAF / Suspense / the lazy engine. The tour auto-launches only
 * on a settled dashboard, once the seen state is known, with a fine pointer,
 * when the user has not yet seen it, and never inside an automated browser
 * session (a synthetic monitor, a scraper, or our own Playwright suites): the
 * spotlight overlay would otherwise intercept clicks in unrelated flows. The
 * menu re-trigger stays available.
 */
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
  /** Launch the tour for the current scope. Ignores the seen flag. */
  startTour: () => void;
  isTourActive: boolean;
  /** Render this somewhere stable in the tree (e.g. at the App root). */
  tourElement: ReactNode;
}

export function useTour({
  scope,
  readOnly,
  isDesktop,
  autoLaunchReady,
  seen,
  seenKnown,
  onSeen,
}: UseTourOptions): UseTourResult {
  const [run, setRun] = useState(false);
  const [steps, setSteps] = useState<TourStep[]>([]);
  const autoStartedRef = useRef(false);
  const mountedRef = useRef(true);
  const [trackedScope, setTrackedScope] = useState(scope);

  // Mount/cleanup tracking
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  // Render-time: cancel tour when scope changes
  if (trackedScope !== scope) {
    setTrackedScope(scope);
    setRun(false);
  }

  // Defer one frame so a closing menu / freshly committed route has painted
  // before we probe the DOM for anchors. No arbitrary timeout. `onStarted`
  // fires synchronously, only when the tour actually starts (steps resolved),
  // so callers can latch on real success rather than on the scheduling attempt.
  const begin = useCallback(
    (onStarted?: () => void): number => {
      return requestAnimationFrame(() => {
        if (!mountedRef.current) return;
        const resolved = resolveTourSteps({ scope, readOnly, isDesktop });
        if (resolved.length === 0) return;
        onStarted?.();
        setSteps(resolved);
        setRun(true);
      });
    },
    [scope, readOnly, isDesktop],
  );

  const startTour = useCallback(() => {
    begin();
  }, [begin]);

  // Auto-launch: once per mount, dashboard scope, fine pointer, seen state
  // known and unset. Uses refs so the effect can run once on mount while
  // reading the latest prop values.
  const autoLaunchReadyRef = useRef(autoLaunchReady);
  const seenKnownRef = useRef(seenKnown);
  const scopeRef = useRef(scope);
  const isDesktopRef = useRef(isDesktop);
  const seenRef = useRef(seen);
  const beginRef = useRef(begin);
  useEffect(() => {
    autoLaunchReadyRef.current = autoLaunchReady;
    seenKnownRef.current = seenKnown;
    scopeRef.current = scope;
    isDesktopRef.current = isDesktop;
    seenRef.current = seen;
    beginRef.current = begin;
  }, [autoLaunchReady, seenKnown, scope, isDesktop, seen, begin]);
  useEffect(() => {
    if (autoStartedRef.current) return;
    const automated = isAutomatedSession();
    if (
      !shouldAutoLaunch({
        autoLaunchReady,
        seenKnown,
        scope,
        isDesktop,
        seen,
        automated,
      })
    )
      return;
    const id = beginRef.current(() => {
      autoStartedRef.current = true;
    });
    return () => cancelAnimationFrame(id);
  }, [autoLaunchReady, seenKnown, scope, isDesktop, seen]);

  const handleFinish = useCallback(
    (markSeen: boolean) => {
      setRun(false);
      if (markSeen) onSeen();
    },
    [onSeen],
  );

  const tourElement = run ? (
    <Suspense fallback={null}>
      <TourRunner run={run} steps={steps} onFinish={handleFinish} />
    </Suspense>
  ) : null;

  return { startTour, isTourActive: run, tourElement };
}

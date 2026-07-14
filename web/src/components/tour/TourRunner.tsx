// The only module that imports react-joyride. Lazy-loaded by TourProvider the
// first time a tour actually runs, so returning users (who have the
// `aoe-tour-seen` flag set) never download the engine. Everything react-joyride
// specific (the component, its event/action constants, theming) lives here;
// TourProvider stays engine-agnostic and deals only in TourStep data. Swapping
// the engine later means rewriting this file alone.
//
// Controlled mode (`stepIndex`) is load-bearing for the settings-modal steps. A
// step with `settingsTab` lives behind the route-driven Settings modal, so its
// anchor only exists after we navigate there. If react-joyride ran uncontrolled
// it would query the target the instant it advanced, before React committed the
// new route, and fire TARGET_NOT_FOUND. Instead we own the index: on every
// transition we ask the host to navigate, unmount Joyride while the DOM mutates,
// poll for the target anchor, and only then remount at the new index. Same path
// serves Next and Back, so cross-modal navigation can never hand Joyride a
// missing target.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ACTIONS, Joyride, EVENTS, STATUS, type EventData, type Step } from "react-joyride";
import { type TourShortcutHint, type TourStep, tourSelector } from "../../lib/tourSteps";
import { SHORTCUTS_BY_ID, formatTourShortcut } from "../../lib/shortcuts";
import { TOUR_RUNNER_OPTIONS, TOUR_RUNNER_STYLES } from "./tourRunnerStyles";

/** Settings tab a step opens, or null to close settings and return to base. */
export type TourSettingsTab = NonNullable<TourStep["settingsTab"]>;

export interface TourRunnerProps {
  run: boolean;
  steps: TourStep[];
  /** Called once when the tour ends. `markSeen` is false for our own programmatic
   *  stop (scope change / unmount), true for a user finish, skip, or close. */
  onFinish: (markSeen: boolean) => void;
  /** Open the given Settings tab, or close Settings when passed null. Drives the
   *  route so the deferred anchor of a settingsTab step mounts before its step. */
  onNavigate: (tab: TourSettingsTab | null) => void;
}

const LOCALE = { skip: "Skip", last: "Done", next: "Next", back: "Back" };

// Safety net for the poll: settings tabs are core, always-present UI, so a
// missing anchor here means a route crash or a removed tab, not a normal state.
const ANCHOR_WAIT_MS = 3000;

function hintLine(hint: TourShortcutHint): string {
  return `${formatTourShortcut(SHORTCUTS_BY_ID[hint.id].chord)} ${hint.verb}`;
}

function StepBody({ body, shortcutHints }: { body: string; shortcutHints?: readonly TourShortcutHint[] }) {
  return (
    <div>
      <p>{body}</p>
      {shortcutHints && shortcutHints.length > 0 && (
        <ul className="mt-2 space-y-0.5 text-[11px] text-text-muted">
          {shortcutHints.map((hint) => (
            <li key={`${hint.id}:${hint.verb}`} className="font-mono">
              {hintLine(hint)}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function toJoyrideStep(step: TourStep): Step {
  return {
    id: step.id,
    target: tourSelector(step.anchor),
    title: step.title,
    content: <StepBody body={step.body} shortcutHints={step.shortcutHints} />,
    placement: "auto",
    // Skip scroll-into-view when the anchor is already in view but the tab grows
    // async: react-joyride otherwise loops on scroll and never advances (#2631).
    ...(step.disableScrolling ? { skipScroll: true } : {}),
  };
}

export default function TourRunner({ run, steps, onFinish, onNavigate }: TourRunnerProps) {
  const joyrideSteps = useMemo(() => steps.map(toJoyrideStep), [steps]);
  const [stepIndex, setStepIndex] = useState(0);
  // While suspended we unmount Joyride so it neither fires TARGET_NOT_FOUND nor
  // paints a tooltip against a node that is mid-unmount (e.g. the sidebar going
  // away as Settings opens). The poll below lifts it once the next anchor lands.
  const [suspended, setSuspended] = useState(false);

  // Single terminal exit. react-joyride can report the end of a tour more than
  // once (e.g. Escape on the last step fires STEP_AFTER with action=CLOSE and
  // TOUR_END with status=FINISHED), so latch the first one and drop the rest to
  // avoid a double onFinish / double "seen" write. A fresh tour is a fresh
  // TourRunner mount, so the ref resets naturally.
  const endedRef = useRef(false);
  const end = useCallback(
    (markSeen: boolean, index: number) => {
      if (endedRef.current) return;
      endedRef.current = true;
      // If we end while parked on a settings step, return to the dashboard so
      // the user does not land stranded in Settings.
      if (steps[index]?.settingsTab) onNavigate(null);
      onFinish(markSeen);
    },
    [steps, onNavigate, onFinish],
  );

  useEffect(() => {
    if (!suspended) return;
    // stepIndex is always in-bounds here: suspension is only ever set alongside
    // setStepIndex(next) after the STEP_AFTER handler bounds-checks `next`. The
    // guard is a plain return (no setState) purely to satisfy the type checker.
    const step = steps[stepIndex];
    if (!step) return;
    const selector = tourSelector(step.anchor);
    const start = performance.now();
    let frame = 0;
    const tick = () => {
      if (document.querySelector(selector)) {
        setSuspended(false);
        return;
      }
      if (performance.now() - start > ANCHOR_WAIT_MS) {
        // ponytail: end the tour rather than hang; it re-triggers from the menu.
        // Upgrade to skip-the-step-in-direction if a feature-flagged (droppable)
        // settings tab is ever added as a target.
        end(false, stepIndex);
        return;
      }
      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  }, [suspended, stepIndex, steps, end]);

  const handleEvent = useCallback(
    (data: EventData) => {
      const { action, index, status, type } = data;
      const terminalStatus = status === STATUS.FINISHED || status === STATUS.SKIPPED;
      // A user dismiss (Escape, the close button, Skip) reaches us as an action.
      // In controlled mode react-joyride leaves status=RUNNING on a non-last
      // close, so gate the dismiss on the action, not the status; otherwise it
      // falls through to STEP_AFTER and advances the tour instead of ending it
      // (#2819). Escape/close arrive as STEP_AFTER(action=CLOSE), not TOUR_END.
      const userDismiss = action === ACTIONS.CLOSE || action === ACTIONS.SKIP;

      if (type === EVENTS.TOUR_END || terminalStatus || userDismiss) {
        // A finish/skip/close is the user's doing (mark it seen). A bare
        // TOUR_END with a non-terminal status and no dismiss action is a
        // programmatic stop (scope change / unmount): do not mark it seen.
        end(terminalStatus || userDismiss, index);
        return;
      }

      if (type === EVENTS.STEP_AFTER) {
        // Only real navigation moves the index. Never default an unhandled
        // action to +1: that is exactly how a CLOSE used to advance the tour.
        if (action !== ACTIONS.NEXT && action !== ACTIONS.PREV) return;
        const direction = action === ACTIONS.PREV ? -1 : 1;
        const next = index + direction;
        // Advancing past the last step is the user finishing (clicking Done). We
        // must end it ourselves: because each settings-tab crossing remounts a
        // fresh Joyride, the engine does not emit its own TOUR_END on the last
        // step in that flow, so relying on it strands the overlay (#2819).
        if (next >= steps.length) {
          end(true, index);
          return;
        }
        if (next < 0) return;
        const currentTab = steps[index]?.settingsTab ?? null;
        const nextTab = steps[next]?.settingsTab ?? null;
        setStepIndex(next);
        // Only the crossings that change the settings route need a navigate +
        // suspend; dashboard-to-dashboard steps advance with no extra work.
        if (currentTab !== nextTab) {
          onNavigate(nextTab);
          setSuspended(true);
        }
        return;
      }

      if (type === EVENTS.TARGET_NOT_FOUND) {
        // Should not happen given the suspend/poll, but never leave the user
        // stuck on a spotlight with no tooltip, or stranded in Settings.
        end(false, index);
      }
    },
    [steps, onNavigate, end],
  );

  if (suspended) return null;

  return (
    <Joyride
      run={run}
      stepIndex={stepIndex}
      steps={joyrideSteps}
      continuous
      options={TOUR_RUNNER_OPTIONS}
      locale={LOCALE}
      styles={TOUR_RUNNER_STYLES}
      onEvent={handleEvent}
    />
  );
}

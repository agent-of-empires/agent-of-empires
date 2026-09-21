// The only module importing react-joyride; lazy-loaded when a tour first runs.
// Controlled `stepIndex` is load-bearing: a `settingsTab` anchor exists only after
// navigating, so each crossing unmounts Joyride, polls for the anchor, then remounts.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ACTIONS, Joyride, EVENTS, STATUS, type EventData, type Step } from "react-joyride";
import { type TourShortcutHint, type TourStep, tourSelector } from "../../lib/tourSteps";
import { SHORTCUTS_BY_ID, formatTourShortcut } from "../../lib/shortcuts";
import { TOUR_RUNNER_OPTIONS, TOUR_RUNNER_STYLES } from "./tourRunnerStyles";

export type TourSettingsTab = NonNullable<TourStep["settingsTab"]>;

export interface TourRunnerProps {
  run: boolean;
  steps: TourStep[];
  /** `markSeen` is false for a programmatic stop, true for a user finish, skip, or close. */
  onFinish: (markSeen: boolean) => void;
  /** Open the given Settings tab, or close Settings when passed null. */
  onNavigate: (tab: TourSettingsTab | null) => void;
}

const LOCALE = { skip: "Skip", last: "Done", next: "Next", back: "Back" };

// Settings tabs always exist, so a longer wait means a crash or removed tab.
const ANCHOR_WAIT_MS = 3000;

function StepBody({ body, shortcutHints }: { body: string; shortcutHints?: readonly TourShortcutHint[] }) {
  return (
    <div>
      <p>{body}</p>
      {shortcutHints && shortcutHints.length > 0 && (
        <ul className="mt-2 space-y-0.5 text-[11px] text-text-muted">
          {shortcutHints.map((hint) => (
            <li key={`${hint.id}:${hint.verb}`} className="font-mono">
              {`${formatTourShortcut(SHORTCUTS_BY_ID[hint.id].chord)} ${hint.verb}`}
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
    // An in-view anchor whose tab grows async otherwise makes joyride loop on scroll.
    ...(step.disableScrolling ? { skipScroll: true } : {}),
  };
}

export default function TourRunner({ run, steps, onFinish, onNavigate }: TourRunnerProps) {
  const joyrideSteps = useMemo(() => steps.map(toJoyrideStep), [steps]);
  const [stepIndex, setStepIndex] = useState(0);
  // Joyride stays unmounted while the next anchor is pending.
  const [suspended, setSuspended] = useState(false);

  // Joyride can report the end more than once; latch the first.
  const endedRef = useRef(false);
  const end = useCallback(
    (markSeen: boolean, index: number) => {
      if (endedRef.current) return;
      endedRef.current = true;
      if (steps[index]?.settingsTab) onNavigate(null);
      onFinish(markSeen);
    },
    [steps, onNavigate, onFinish],
  );

  useEffect(() => {
    if (!suspended) return;
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
      // Controlled mode keeps status RUNNING on a non-last close, so gate on the action.
      const userDismiss = action === ACTIONS.CLOSE || action === ACTIONS.SKIP;

      if (type === EVENTS.TOUR_END || terminalStatus || userDismiss) {
        end(terminalStatus || userDismiss, index);
        return;
      }

      if (type === EVENTS.STEP_AFTER) {
        if (action !== ACTIONS.NEXT && action !== ACTIONS.PREV) return;
        const next = index + (action === ACTIONS.PREV ? -1 : 1);
        // Remounted Joyrides emit no TOUR_END on the last step, so Done ends here.
        if (next >= steps.length) {
          end(true, index);
          return;
        }
        if (next < 0) return;
        const nextTab = steps[next]?.settingsTab ?? null;
        setStepIndex(next);
        if ((steps[index]?.settingsTab ?? null) !== nextTab) {
          onNavigate(nextTab);
          setSuspended(true);
        }
        return;
      }

      if (type === EVENTS.TARGET_NOT_FOUND) end(false, index);
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

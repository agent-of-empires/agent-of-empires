// The only module importing react-joyride; lazy-loaded when a tour first runs.
import { useCallback, useMemo, useRef, useState } from "react";
import { ACTIONS, Joyride, EVENTS, STATUS, type EventData, type Step } from "react-joyride";
import { type TourShortcutHint, type TourStep, tourSelector } from "../../lib/tourSteps";
import { SHORTCUTS_BY_ID, formatTourShortcut } from "../../lib/shortcuts";
import { TOUR_RUNNER_OPTIONS, TOUR_RUNNER_STYLES } from "./tourRunnerStyles";

export interface TourRunnerProps {
  run: boolean;
  steps: TourStep[];
  /** `markSeen` is false for a programmatic stop, true for a user finish, skip, or close. */
  onFinish: (markSeen: boolean) => void;
}

const LOCALE = { skip: "Skip tour", last: "Done", next: "Next", back: "Back" };

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
  };
}

export default function TourRunner({ run, steps, onFinish }: TourRunnerProps) {
  const joyrideSteps = useMemo(() => steps.map(toJoyrideStep), [steps]);
  const [stepIndex, setStepIndex] = useState(0);

  // Joyride can report the end more than once; latch the first.
  const endedRef = useRef(false);
  const end = useCallback(
    (markSeen: boolean) => {
      if (endedRef.current) return;
      endedRef.current = true;
      onFinish(markSeen);
    },
    [onFinish],
  );

  const handleEvent = useCallback(
    (data: EventData) => {
      const { action, index, status, type } = data;
      const terminalStatus = status === STATUS.FINISHED || status === STATUS.SKIPPED;
      // Controlled mode keeps status RUNNING on a non-last close, so gate on the action.
      const userDismiss = action === ACTIONS.CLOSE || action === ACTIONS.SKIP;

      if (type === EVENTS.TOUR_END || terminalStatus || userDismiss) {
        end(terminalStatus || userDismiss);
        return;
      }

      if (type === EVENTS.STEP_AFTER) {
        if (action !== ACTIONS.NEXT && action !== ACTIONS.PREV) return;
        const next = index + (action === ACTIONS.PREV ? -1 : 1);
        // Done on the last step ends here rather than relying on a TOUR_END.
        if (next >= steps.length) {
          end(true);
          return;
        }
        if (next >= 0) setStepIndex(next);
        return;
      }

      if (type === EVENTS.TARGET_NOT_FOUND) end(false);
    },
    [steps, end],
  );

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

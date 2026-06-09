// The only module that imports react-joyride. Lazy-loaded by TourProvider the
// first time a tour actually runs, so returning users (who have the
// `aoe-tour-seen` flag set) never download the engine. Everything react-joyride
// specific (the component, its event/action constants, theming) lives here;
// TourProvider stays engine-agnostic and deals only in TourStep data. Swapping
// the engine later means rewriting this file alone.
import { useCallback, useMemo } from "react";
import {
  Joyride,
  EVENTS,
  STATUS,
  type ButtonType,
  type EventData,
  type Options,
  type Step,
  type Styles,
} from "react-joyride";
import { type TourShortcutHint, type TourStep, tourSelector } from "../../lib/tourSteps";
import { SHORTCUTS_BY_ID, formatTourShortcut } from "../../lib/shortcuts";

export interface TourRunnerProps {
  run: boolean;
  steps: TourStep[];
  /** Called once when the tour ends. `markSeen` is false for our own programmatic
   *  stop (scope change / unmount), true for a user finish, skip, or close. */
  onFinish: (markSeen: boolean) => void;
}

// Theme via the app's resolved-theme CSS variables (web/src/index.css) so the
// tooltip tracks light vs dark instead of being pinned to dark hex. These land
// as inline CSS styles, where var() resolves. The exception is overlayColor: it
// is painted as an SVG fill *attribute*, where var() does not reliably resolve,
// so the scrim stays a literal translucent dark (it reads correctly over both
// light and dark content).
const OPTIONS: Partial<Options> = {
  buttons: ["skip", "back", "primary"] as ButtonType[],
  showProgress: true,
  skipBeacon: true,
  primaryColor: "var(--color-brand-600)",
  overlayColor: "rgba(2, 6, 23, 0.65)",
  textColor: "var(--color-text-primary)",
  zIndex: 10_000,
  scrollOffset: 96,
};

const LOCALE = { skip: "Skip", last: "Done", next: "Next", back: "Back" };

const STYLES: Partial<Styles> = {
  tooltip: {
    backgroundColor: "var(--color-surface-800)",
    border: "1px solid var(--color-surface-700)",
    borderRadius: 10,
    color: "var(--color-text-primary)",
    fontSize: 13,
  },
  tooltipTitle: {
    color: "var(--color-brand-500)",
    fontSize: 14,
    fontWeight: 600,
  },
  tooltipContent: { padding: "10px 4px" },
  buttonPrimary: {
    backgroundColor: "var(--color-brand-600)",
    borderRadius: 6,
    // Dark text reads on the amber primary in both themes; keep it fixed.
    color: "#0f172a",
  },
  buttonBack: { color: "var(--color-text-secondary)" },
  buttonSkip: { color: "var(--color-text-dim)" },
};

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
  };
}

export default function TourRunner({ run, steps, onFinish }: TourRunnerProps) {
  const joyrideSteps = useMemo(() => steps.map(toJoyrideStep), [steps]);

  const handleEvent = useCallback(
    (data: EventData) => {
      if (data.type !== EVENTS.TOUR_END) return;
      // Gate on the terminal status, not the action: a programmatic stop
      // (run -> false on scope change / unmount) ends with a non-terminal
      // status and may carry `action: null`, which an action allowlist would
      // misread as a user finish and silently opt the user out. Only an
      // actual finish or skip marks the tour seen.
      const markSeen = data.status === STATUS.FINISHED || data.status === STATUS.SKIPPED;
      onFinish(markSeen);
    },
    [onFinish],
  );

  return (
    <Joyride
      run={run}
      steps={joyrideSteps}
      continuous
      options={OPTIONS}
      locale={LOCALE}
      styles={STYLES}
      onEvent={handleEvent}
    />
  );
}

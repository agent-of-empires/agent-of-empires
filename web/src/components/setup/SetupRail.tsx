import { Check, Minus } from "lucide-react";
import type { SetupStep } from "../../lib/api";
import { SETUP_STEP_INFO } from "../../lib/setup";

function stepNote(step: SetupStep): { text: string; tone: string } {
  if (step.configured) return { text: "Done", tone: "text-status-running" };
  if (step.skipped) return { text: "Skipped", tone: "text-text-dim" };
  return { text: step.required ? "Required" : "Optional", tone: "text-text-dim" };
}

interface Props {
  steps: SetupStep[];
  current: number;
  onSelect: (index: number) => void;
}

/** Non-linear step list: every step is reachable, done or not. */
export function SetupRail({ steps, current, onSelect }: Props) {
  return (
    <ol className="flex flex-col gap-1">
      {steps.map((step, i) => {
        const note = stepNote(step);
        const active = i === current;
        return (
          <li key={step.key}>
            <button
              type="button"
              aria-current={active ? "step" : undefined}
              onClick={() => onSelect(i)}
              className={`w-full flex items-center gap-3 rounded-md border px-3 min-h-11 py-1.5 text-left cursor-pointer transition-colors ${active ? "border-brand-600/70 bg-surface-800" : "border-transparent hover:bg-surface-800/60"}`}
            >
              <span className="w-4 shrink-0 flex justify-center font-mono text-sm text-brand-500">
                {step.configured ? (
                  <Check size={14} className="text-status-running" />
                ) : step.skipped ? (
                  <Minus size={14} className="text-text-dim" />
                ) : (
                  i + 1
                )}
              </span>
              <span className="flex flex-col">
                <span className="text-sm font-medium text-text-primary">{SETUP_STEP_INFO[step.key].title}</span>
                <span className={`text-xs ${note.tone}`}>{note.text}</span>
              </span>
            </button>
          </li>
        );
      })}
    </ol>
  );
}
